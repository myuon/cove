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
//! - **`trim()` trimmed Unicode whitespace** and **`words()` split on ASCII
//!   whitespace**, which is the pair the oracle had and was not a distinction
//!   this file invented. Neither is here any more — issue #454's Step 5 made
//!   both of them `std.string` bodies — and the pair went *together* for that
//!   difference rather than for anything they shared: twenty of `trim`'s
//!   twenty-five code points are ordinary characters to `words`, and the
//!   vertical tab is one ASCII byte that is in one set and not the other. The
//!   two `std.string` bodies have no predicate in common, and
//!   `tests/e2e/values_string_trim` and `tests/e2e/values_string_words` sweep
//!   the same six bands to say so.
//! - **`toUpper()` and `toLower()` were full Unicode case mappings**, so the
//!   answer could be longer than what it was called on. They are not here any
//!   more either — issue #454's Step 5 finished with them — and what moved is
//!   the *table*: 1,580 uppercase mappings and 1,488 lowercase ones, 102 of
//!   them one-to-many, packed into six string literals in `std.string` that
//!   `crates/cove-sema/tests/unicase.rs` generates and holds byte-identical.
//!   `toLower` took a **context** rule with it, which nothing else in this
//!   file ever had: `Σ` is `ς` at the end of a word and `σ` elsewhere.
//!
//! Every one of those is `crates/cove-runtime/src/builtins.rs`'s reading. The
//! bytes are decoded into a Rust `String` once and the operation runs on
//! that, so there is exactly one place either backend could be reading them
//! differently, and it is this sentence.
//!
//! # Every operation here says what it examined, in bytes
//!
//! Three of the operations below walk the whole receiver — it was seven, and
//! `toUpper` and `toLower` are the two that left most recently — and until
//! [ADR 0064](../../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
//! Decision 7 every one of them cost the run **one** unit of work — the one
//! an `add.int` costs. So `"ab".replace("a", "b")` and a `replace` over a
//! hundred thousand characters spent the same fuel, ran for hundreds of times
//! the wall clock, and were the same to a cancellation, a deadline and a fuel
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
//! - the ones that walk the receiver — `split` and `replace` — charge the
//!   receiver's own byte length, because
//!   [`operand::text`] has already decoded the whole of it before any of them
//!   looks at a single character. `slice` was one of them and charged the same
//!   way — the receiver's bytes and not the answer's, because a two-character
//!   slice of a megabyte read the megabyte. In `std.string` it charges what it
//!   *walks*, an instruction a character as far as `to` and not one byte
//!   further, which is the same trade ADR 0065 made for the searches;
//! - `words` and `chars` walked the receiver too, and both are `std.string`
//!   now. `trim` is the one whose *charge* the move changed most, and not only
//!   in which mechanism pays it: the arm here charged the whole receiver's
//!   length whatever it found, so `"abc".trim()` on a 170-byte line was
//!   charged 170 bytes of bulk work for reading two. `std.string.trim` walks
//!   in from each end and stops at the first character that is not whitespace,
//!   so what it pays for is what it looked at — which on `examples/cq` is
//!   16,591,749 units of this column replaced by 3,300,000 instructions;
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
//! seventeen variants rather than the thirteen it planned for. (ADR 0067
//! answered #461 with `core.refuse`, and both are `std.string` bodies now;
//! `StringRefuseByteRange` is the one this module still holds.)
//!
//! **Step 4 found the same wall at a different receiver and went round it.**
//! `Int.parse` and `Int.parseRadix` were to move together, the general one
//! first and the decimal one as a radix-10 wrapper over it; `int_parse_radix`
//! raises on a radix outside `2..=36`, so the order inverted and `parse` alone
//! became `std.int.parse` — which never raises, because every failure it has
//! is an `Err` value it builds. Sixteen variants, and #461 now holds six of
//! them. (It was decided as ADR 0067, and `parseRadix` is `std.int.parseRadix`
//! now too.)
//!
//! **Step 5 took two that #461 does not touch**, and that is the whole of why
//! it could. Neither `trim` nor `words` refuses anything: every input is text
//! and every answer is text, so a Cove body needs nothing to raise with.
//! Fourteen variants, and #461 still holds six of them — which is now **six
//! of fourteen** rather than six of sixteen, and the fraction is the shape of
//! what is left. What #461 blocks is most of the remainder.
//!
//! `tests/e2e/values_string_split` and `tests/e2e/values_string_replace` pin
//! what the two operations answer, and `tests/e2e/fail_string_split_empty` and
//! `tests/e2e/fail_string_replace_empty` pin what they refuse, word for word —
//! including that the two `help` lines are **not** the same sentence.

use crate::error::RuntimeError;
use crate::vm::exec::Machine;
use crate::vm::intrinsics::operand;
use crate::vm::intrinsics::operand::Frame;

// `words` was here, above `split`, and its *contract* was the interesting
// thing about it rather than its algorithm. It was
// `text.split_ascii_whitespace().collect()` — five bytes, tab, line feed,
// form feed, carriage return and space, and **not** the vertical tab, which
// is what a reader who wrote "the ASCII control characters and a space" would
// have got wrong. `std.string.words` is a byte scan over `byteAt` with one
// `core.stringSlice` a part, and it needs no decode at all: a byte below 128
// is a character of its own in UTF-8, so a scan that compares bytes finds
// exactly the separators a scan of characters would and every boundary it
// cuts at is a character boundary. Issue #454's Step 5.

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

// `split` was here, out of `str::split` and `make::strings`, and it was the
// last arm in this file that answered an array. It is `std.string.split` now —
// a `core.stringFind` a separator and a `core.stringSlice` a part, into a
// `Vector` sized by a first pass of the same searches — and it raises on an
// empty separator through ADR 0067's `core.refuse`, which is the one thing that
// kept it below while `words` and `chars` moved. Issue #454's Step 3, finished.

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

// `trim` was here, and what left with it is a **Unicode version**. It was
// `str::trim`, so the set it removed was whatever `char::is_whitespace`
// answered in the Rust toolchain this binary happened to be built with, and
// nothing in the repository recorded which that was: a toolchain bump that
// moved a code point would have changed what every Cove program meant, with
// no diff to see it in. ADR 0064's Decision 5 asks that Cove own its Unicode
// version, and `std.string.trim` does — the twenty-five `White_Space` code
// points are written out as UTF-8 byte patterns with each one named and the
// version stated, `crates/cove-runtime/tests/unicode.rs` sweeps all of
// `0 ..= 0x10FFFF` and holds that set to the toolchain's, and the failure of
// that test is what a future bump produces instead of silence.
//
// `toUpper` and `toLower` were the half of issue #454's Step 5 that could not
// go with it, and the reason was the size of the table rather than anything
// about the operations. They went next, with the asset: **10,969 bytes** in
// six `std.string` literals, generated by `crates/cove-sema/tests/unicase.rs`
// and held byte-identical by it, read by a binary search over records of
// thirteen base-32 symbols. `tests/e2e/values_string_to_upper` and
// `tests/e2e/values_string_to_lower` are 499 golden lines each, against an
// oracle built from Unicode 17.0.0's own published files.
//
// `toLower` is the one operation that ever left this file with a rule that
// depends on a character's **neighbours**: `Σ` lowercases to `ς` at the end of
// a word and to `σ` elsewhere, and what "a word" means is two more Unicode
// properties that *overlap* — 268 code points are in both, and the scan skips
// before it asks, so each of those counts as ignorable. `std.string.sigmaIsFinal`
// is where that is argued.

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

// `replace` was here, out of `str::replace`, and it was the last operation in
// this file that searched. It is `std.string.replace` now: the matches counted
// with `core.stringFind`, the answer allocated once at the length that count
// gives, and every run between matches and every copy of `new` one append
// window. Text with nothing to replace is answered as it is, where this arm
// built a copy. It raises on an empty `old` the way `split` does.

#[cfg(test)]
mod tests {
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

    // `chars` had a line here — `"hé"` into `["h", "é"]` — until issue #454's
    // Step 3 moved it to `std.string.chars`. `tests/e2e/values_string_chars`
    // replaced it with 41 calls on both evaluators against a golden a
    // standalone `rustc` oracle wrote, and the difference is not the count:
    // the oracle has its own UTF-8 encoder and its own lead-byte splitter, so
    // every width boundary, a NUL, and nine rows of combining marks, joiners
    // and modifiers are checked against an implementation this repository does
    // not ship, where the one case here was checked against the arm that
    // answered it.
    //
    // A `words_splits_on_ascii_whitespace` case stood beside it, asserting
    // that `"  one  two "` answered `["one", "two"]`. `words` is not an arm in
    // this file any more either, and what replaced that one case is
    // `tests/e2e/values_string_words`: 382 golden lines on both evaluators, of
    // which 274 are a **sweep** that asks every code point in six bands
    // whether it separates. The case here could not have caught the thing that
    // sweep is for — the vertical tab is `White_Space` and is not one of the
    // five bytes — because it held one ASCII string with spaces in it.

    // A `split_separates_on_the_separator_and_refuses_an_empty_one` case
    // stood here — `"a,,b"` into `["a", "", "b"]`, and the empty separator's
    // message and help — and a `replace_rewrites_every_match_and_refuses_an_empty_old`
    // case further down, `"banana"` to `"bbbnbbnbb"`. Neither operation is an
    // arm in this file any more, and what answers both questions is
    // `tests/e2e/values_string_split`, `values_string_replace`,
    // `fail_string_split_empty` and `fail_string_replace_empty`, written in
    // #466 before either moved and asked of both evaluators.

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

    // `trim` had the first line of this test and it is gone, and what it
    // asserted is worth writing down beside what replaced it. It was
    // `"\u{a0} a \n"` trimming to `"a"` — one row, chosen because a no-break
    // space is `White_Space` and is not ASCII, so the line said "this is
    // Unicode's set and not ASCII's" and nothing more. Issue #454's Step 5
    // made the body `std.string.trim`, and `tests/e2e/values_string_trim`
    // replaced the line with 347 golden lines on both evaluators, 274 of them
    // a sweep over six bands that brackets every run of `White_Space` with
    // ordinary characters — so the set is under test at every boundary rather
    // than at one member of it, against a golden a standalone `rustc` oracle
    // wrote from a list of its own.
    //
    // The two case mappings that stood here have gone the same way, and they
    // were not re-pointed at anything. `straße` to `STRASSE` and `ÉÀ` to `éà`
    // were assertions about `str::to_uppercase` and `str::to_lowercase`, so
    // there is no arm left in this file for them to be about; a version of
    // them written against `std.string.toUpper` would be the *subject*
    // answering for itself, which is what
    // `tests/e2e/values_string_to_upper` and `tests/e2e/values_string_to_lower`
    // are for. Those two files carry both rows — `sharp.s` and
    // `accents.upper` — among 499 lines apiece, against an oracle built from
    // Unicode's published data, and `crates/cove-runtime/tests/unicode.rs`
    // sweeps every code point from 0 to `0x10FFFF` in both directions. The
    // oracle is stronger than it was, and it is somewhere else.

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

    // `split_holds_the_array_it_is_filling` stood last: the array a `split()`
    // built was a root while it was filled, through `make::strings`' temporary
    // root, and a heap full of dead objects forced a collection partway
    // through to prove it. The question outlived the arm, as it outlived
    // `chars` before it: `std.string.split` fills a `Vector` the frame holds,
    // and `vm::differential`'s
    // `a_split_that_collects_keeps_the_parts_it_has_made` asks it of that body
    // under heap pressure, on both evaluators.
}
