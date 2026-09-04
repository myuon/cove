//! The C ABI a JavaScript embedder calls, and the two allocation primitives
//! that make it usable.
//!
//! # Why there is no `wasm-bindgen`
//!
//! This workspace has exactly one third-party dependency (`toml`, in
//! `cove-sema`); the CLI parses its own arguments and writes its own JSON. A
//! binding generator would be the largest dependency in the tree, and a probe
//! established before any of this was written that it buys nothing here: four
//! `extern "C"` functions and a length prefix are the whole of what crossing
//! this boundary needs.
//!
//! # The calling convention
//!
//! Two directions, one shape each.
//!
//! *Into* the module: the caller asks for `n` bytes with [`cove_alloc`],
//! writes UTF-8 into the module's exported `memory` at the returned offset,
//! and passes `(offset, n)`. It owns those bytes and releases them with
//! [`cove_free`]; nothing here takes them.
//!
//! *Out of* the module: [`cove_compile`] and [`cove_run`] each answer one
//! offset into the same memory. The four bytes there are a little-endian
//! `u32` length, and the `n` bytes after them are UTF-8 JSON. The caller
//! decodes them and releases the whole block with `cove_free(offset, n + 4)`.
//!
//! The length prefix is what removes the alternative — a second exported
//! function answering "how long was the last answer?" — and with it the
//! module-level state such a function would need. Two calls in flight at once
//! would have raced over it. This has nothing to race over.
//!
//! # The one import
//!
//! `cove.cove_now_millis() -> f64`, described by [`cove_runtime`]'s
//! `wallclock` module. It is the monotonic clock, and without it a deadline
//! could not be enforced. A module instantiated without it does not load.

use std::alloc::{alloc, dealloc, Layout};

use crate::{compile_json, run_json};

/// Reserves `len` bytes of the module's memory and answers where they start.
///
/// Byte-aligned, because everything that crosses this boundary is UTF-8 or a
/// little-endian integer read a byte at a time by a `DataView`.
///
/// A zero-length request answers a non-null aligned offset that must still be
/// passed back to [`cove_free`], so a caller never has to special-case the
/// empty string.
///
/// # Safety
///
/// The caller owns the returned block until it hands it to [`cove_free`].
/// Nothing else in this module will free it.
#[no_mangle]
pub extern "C" fn cove_alloc(len: usize) -> *mut u8 {
    // SAFETY: the alignment is 1, which is non-zero and a power of two, and
    // `len` rounded up to it cannot overflow because it is already a
    // multiple of it.
    unsafe {
        let layout = Layout::from_size_align_unchecked(len.max(1), 1);
        alloc(layout)
    }
}

/// Releases a block [`cove_alloc`] answered, or an answer block one of the
/// entry points answered.
///
/// For an answer block, `len` is the four-byte prefix plus the length it
/// holds — the JavaScript side has both numbers by the time it decodes the
/// payload, so asking it for the total is cheaper than storing the total here.
///
/// # Safety
///
/// `ptr` must have come from [`cove_alloc`] or from an entry point of this
/// module, `len` must be the size that block was created with, and the block
/// must not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn cove_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    dealloc(ptr, Layout::from_size_align_unchecked(len.max(1), 1));
}

/// Checks and lowers `source`, and answers the diagnostics and the
/// disassembly as an answer block. See [`crate::compile_json`].
///
/// # Safety
///
/// `source` must point at `len` initialized bytes inside this module's
/// memory. They are read and not retained; the caller still owns them.
#[no_mangle]
pub unsafe extern "C" fn cove_compile(source: *const u8, len: usize) -> *mut u8 {
    answer(compile_json(&read(source, len)))
}

/// Checks, lowers and runs `source`, and answers what it printed, what it
/// produced and how it ended. See [`crate::run_json`].
///
/// `fuel` and `deadline_ms` are the two bounds a page can put on a run; zero
/// means "whatever [`crate::RUN_LIMITS`] says", which is a bound and not the
/// absence of one. A deadline is enforced against the imported clock, so it
/// does what it says.
///
/// They are `u32` and not `u64` although [`cove_runtime::Limits`] counts fuel
/// in `u64`, because a `u64` parameter is a wasm `i64`, and a wasm `i64`
/// reaches JavaScript as a `BigInt`: every caller would have to write `10n`
/// where it means ten. Four billion units of fuel is far past what a tab
/// should spend before a page decides it has hung, and forty-nine days is
/// past what a deadline in a browser can mean, so nothing is lost that the
/// awkwardness would buy back.
///
/// # Safety
///
/// As [`cove_compile`].
#[no_mangle]
pub unsafe extern "C" fn cove_run(
    source: *const u8,
    len: usize,
    fuel: u32,
    deadline_ms: u32,
) -> *mut u8 {
    answer(run_json(
        &read(source, len),
        (fuel != 0).then_some(u64::from(fuel)),
        (deadline_ms != 0).then_some(u64::from(deadline_ms)),
    ))
}

/// The caller's bytes as a string.
///
/// Lossy rather than refusing: the source of a playground is whatever the
/// page's `TextEncoder` produced, and a replacement character in a string
/// literal is a thing the parser can report a span for, while "your bytes
/// were not UTF-8" is not.
///
/// # Safety
///
/// As [`cove_compile`].
unsafe fn read(source: *const u8, len: usize) -> String {
    if source.is_null() || len == 0 {
        return String::new();
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(source, len)).into_owned()
}

/// Copies `json` into a freshly allocated length-prefixed block and answers
/// where it starts.
fn answer(json: String) -> *mut u8 {
    let bytes = json.into_bytes();
    // A length that does not fit in the prefix cannot be described to the
    // caller at all, and a truncated answer would be a lie about what the
    // run did. Nothing this module produces approaches 4 GiB; if something
    // ever did, the empty object is the one answer that cannot be
    // misread as a result.
    let len = match u32::try_from(bytes.len()) {
        Ok(len) => len,
        Err(_) => return answer("{}".to_string()),
    };
    let total = bytes.len() + 4;
    let ptr = cove_alloc(total);
    if ptr.is_null() {
        return ptr;
    }
    // SAFETY: `cove_alloc` answered `total` bytes and both writes are inside
    // them.
    unsafe {
        std::ptr::copy_nonoverlapping(len.to_le_bytes().as_ptr(), ptr, 4);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(4), bytes.len());
    }
    ptr
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips a string through the ABI the way a page does: allocate,
    /// write, call, decode the prefix, free both blocks.
    ///
    /// This is the test that would catch the prefix being written big-endian,
    /// or the payload starting at the wrong offset — the two mistakes that a
    /// browser reports as mojibake and nothing else.
    #[test]
    fn an_answer_is_a_little_endian_length_and_then_that_many_bytes() {
        let source = "export fn main() -> Int { 1 }";
        let held = cove_alloc(source.len());
        // SAFETY: `held` names `source.len()` bytes just reserved.
        unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), held, source.len()) };

        // SAFETY: `held` names `source.len()` initialized bytes.
        let answer = unsafe { cove_compile(held, source.len()) };
        // SAFETY: an answer block starts with four length bytes.
        let len = u32::from_le_bytes(
            unsafe { std::slice::from_raw_parts(answer, 4) }
                .try_into()
                .expect("four bytes are four bytes"),
        ) as usize;
        // SAFETY: the block holds `len` payload bytes after the prefix.
        let json =
            String::from_utf8(unsafe { std::slice::from_raw_parts(answer.add(4), len).to_vec() })
                .expect("the answer is UTF-8");

        assert!(json.starts_with('{'), "{json}");
        assert!(json.contains("\"ir\""), "{json}");
        assert_eq!(json.len(), len, "the prefix counts the payload");

        // SAFETY: both blocks came from `cove_alloc` with these sizes.
        unsafe {
            cove_free(held, source.len());
            cove_free(answer, len + 4);
        }
    }

    #[test]
    fn an_empty_source_is_read_rather_than_refused() {
        // SAFETY: a null pointer with a zero length is the empty string.
        let source = unsafe { read(std::ptr::null(), 0) };
        assert_eq!(source, "");
    }
}
