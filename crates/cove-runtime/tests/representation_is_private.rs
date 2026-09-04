//! ADR 0028 decision 0's rule, as ADR 0031 restates it, run as a test
//! instead of kept as a discipline.
//!
//! ADR 0028 names five representations and makes exactly one of them public.
//! The sentence that makes issue #197's thesis — "changing the VM's internal
//! representation must not require exposing that representation to
//! embedders" — true by construction rather than by care is its visibility
//! column. ADR 0028 wrote it as
//!
//! > No public signature in this workspace mentions a `Slot`, a
//! > `HeapObject`, a `Dynamic`, a layout id, a witness, a handle, a frame
//! > base, or a tag.
//!
//! and added that it "is checkable — it is a `grep` over `pub fn`". Two of
//! those words were wrong, both load-bearing, and ADR 0031 supersedes the
//! sentence and nothing else in ADR 0028:
//!
//! > No public signature in `cove-runtime` names a slot, a heap object, a
//! > dynamic value, a witness, a layout id, a frame base, a tag, or a handle
//! > into storage the VM owns.
//!
//! This file is that rule, run by `cargo test`, because a rule nobody runs
//! is a rule that rots. When the first `pub fn` — or `pub trait` method —
//! hands a piece of a private representation to an embedder, it fails here
//! rather than shipping.
//!
//! # The two handles
//!
//! A **VM heap handle** is a reference into storage `cove-runtime` allocated
//! and manages: today a linear address (a bare `u64`) into the run's heap,
//! rooted for as long as anything outside the machine needs it by
//! `crate::vm::mem::Rooted`, a `pub(crate) struct`. It is part of
//! representation 3 and is never public.
//!
//! A **host resource handle** is not one. ADR 0013 decided that it is a
//! *name* — the name of something the host owns, where "every field of it is
//! part of the name" — and ADR 0028's own `ValueView` sketch hands one to an
//! embedder as `Resource(&'a ResourceHandle)`. So does a
//! `std::thread::JoinHandle`, which is how ADR 0008's spawned task is
//! joined, and which the standard library owns rather than this workspace.
//!
//! Neither is an exception to the rule; both are outside it. That is why
//! there is no allowlist in this file. There used to be one, of seven exact
//! signature strings, and every entry in it existed because ADR 0028 wrote
//! "handle" when it meant one of the two.
//!
//! # A word that crosses inward and roots nothing is not a handle
//!
//! `vm::debug::Stop::object` takes a bare `u64` and answers a rendered
//! snapshot, and it passes this scan because it names none of the words
//! below. It is worth saying why it *should* pass, because a grep that
//! happens to be satisfied teaches a reader nothing.
//!
//! The definition above is the argument. A VM heap handle is a reference
//! into storage this crate manages **and roots for as long as anything
//! outside the machine needs it**. `Stop::object` roots nothing: the word
//! goes in, a snapshot of owned strings comes out, and a word that does not
//! name an object this memory holds answers `None` rather than anything at
//! all. The `Stop` it hangs off is borrowed for the length of one
//! `Debugger::at` call and cannot outlive it, so there is no moment at which
//! something outside the machine holds a piece of the heap.
//!
//! The distinction the rule is drawn for survives, too. ADR 0028's sentence
//! is that changing the VM's representation must not require exposing that
//! representation to embedders — to code that *computes* with Cove values. A
//! debugger does not: it renders for a person, in strings it cannot feed
//! back. What it does inherit is the weaker promise. If a linear address
//! stopped being a word index tomorrow, no signature here would move and no
//! debugger UI would fail to compile; the numbers a person reads would mean
//! something else. That is the right trade for a tool whose entire purpose
//! is to look at the representation, and it is a trade rather than an
//! oversight.
//!
//! # `pub fn` is not all of the public surface
//!
//! A trait method carries no visibility of its own — it is exactly as public
//! as its trait, and the trait says `pub` on a different line. So this reads
//! `pub trait` blocks too. `Callable::call_value` was added and reshaped by
//! PR #201 while a `pub fn` scan watched and said nothing, and extending the
//! scan immediately turned up `HostApi::call_resource`, public since ADR
//! 0013 and never once seen.
//!
//! # What it reads, and why that is `cove-runtime` and not the workspace
//!
//! ADR 0028 said "this workspace"; ADR 0031 narrows it to `cove-runtime`,
//! which is where this has always looked. The reason is the visibility
//! column the rule comes from: representations 2, 3 and 4 — `Slot`,
//! `HeapObject`, `Dynamic` — are listed as *private to `cove-runtime`*, and
//! `Value` is listed as the one public thing. `cove-ir` names slots publicly
//! and always has, deliberately: ADR 0019's "Slots, not names" and ADR
//! 0027's places are the *lowering's* vocabulary, decided before ADR 0028
//! and untouched by it, and a lowered program is not something an embedder
//! is handed a piece of.
//!
//! # It is still exact in both directions
//!
//! The old allowlist failed both when an unlisted signature appeared *and*
//! when a listed one disappeared, so that an exception which quietly stops
//! applying is not one nobody re-reads. That is the convention
//! `REGISTERED_REFUSALS` uses in the differential suite. It is kept here at
//! the level of the *category*: each of the two kinds of handle the VM does
//! not own must still be exercised by at least one public signature, or the
//! category has stopped being needed and somebody is made to re-read it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The vocabulary of the four private representations, as ADR 0028 lists it
/// and ADR 0031 restates it, minus the one word that needs resolving rather
/// than matching.
///
/// Matched case-insensitively against the identifiers of a public
/// signature, so `slot` catches `SlotRoots`, `slot`, and `frame_slot` alike.
/// That is deliberately blunt: a name close enough to be caught is a name
/// close enough to want a sentence about why it is allowed.
const FORBIDDEN: &[&str] = &[
    "slot",
    "heapobject",
    "dynamic",
    "witness",
    "layout",
    "frame_base",
    "framebase",
    "tag",
];

/// The eighth word. It is in the rule, and it is the one word that names two
/// different things, so [`names_a_vm_handle`] resolves it instead of
/// [`FORBIDDEN`] matching it.
const HANDLE: &str = "handle";

/// The only handle-named type `cove-runtime` may declare `pub`.
///
/// This is the invariant that keeps the resolution honest. A VM heap handle
/// is private by decision 0's visibility column — `Handle`, `HandleRoots`,
/// `HandleCollection` and `HandleHeap` are all `pub(crate)` — so a
/// handle-named type this crate publishes is, by construction, not one of
/// the four private representations. If that ever stops being true, it stops
/// being true here, at the declaration, rather than later at whichever
/// signature carries it out.
const PUBLIC_HANDLE_TYPES: &[&str] = &["ResourceHandle"];

#[test]
fn no_public_signature_names_an_internal_representation() {
    let offenders = public_signatures_naming_a_representation();
    assert!(
        offenders.is_empty(),
        "a public signature in `cove-runtime` names an internal \
         representation, which ADR 0028 decision 0 forbids and ADR 0031 \
         restates:\n  {}\n\nThere is no allowlist to add it to. Either the \
         signature stops naming a representation, or ADR 0031's rule is \
         superseded again.",
        offenders
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// ADR 0031's invariant: the only handle-named type this crate publishes is
/// the host's, so every other handle-named type it declares is private and
/// unreachable from a public signature.
#[test]
fn the_only_public_handle_type_is_the_host_s() {
    let declared = crate_public_handles();
    let expected: BTreeSet<String> = PUBLIC_HANDLE_TYPES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(
        declared, expected,
        "`cove-runtime`'s public handle-named types are not the ones ADR \
         0031 says they are. A VM heap handle is private to this crate; a \
         host resource handle (ADR 0013) is the one a host is handed."
    );
}

/// The other half of "exact in both directions": each category of handle the
/// VM does not own must still be reached by a public signature, or the
/// sentence in ADR 0031 that permits it has stopped being read.
#[test]
fn both_permitted_kinds_of_handle_are_still_reached() {
    let mut host = 0usize;
    let mut standard_library = 0usize;
    for path in sources() {
        let text = read(&path);
        let from_std = std_handle_types(&text);
        for signature in signatures(&text) {
            for name in identifiers(&signature) {
                if PUBLIC_HANDLE_TYPES.contains(&name.as_str()) {
                    host += 1;
                } else if from_std.contains(&name) {
                    standard_library += 1;
                }
            }
        }
    }
    assert!(
        host > 0,
        "no public signature names a `ResourceHandle` any more, so ADR \
         0031's host-resource-handle category is unused and wants re-reading"
    );
    assert!(
        standard_library > 0,
        "no public signature names a standard-library handle any more, so \
         ADR 0031's standard-library category is unused and wants re-reading"
    );
}

/// Every public signature of `cove-runtime` that names one of the private
/// representations.
fn public_signatures_naming_a_representation() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut scanned = 0usize;
    let published = crate_public_handles();
    for path in sources() {
        let text = read(&path);
        let mut permitted = published.clone();
        permitted.extend(std_handle_types(&text));
        for signature in signatures(&text) {
            scanned += 1;
            let words = signature.to_ascii_lowercase();
            if FORBIDDEN.iter().any(|word| words.contains(word))
                || names_a_vm_handle(&signature, &permitted)
            {
                found.insert(signature);
            }
        }
    }
    assert!(
        scanned > 100,
        "the scan read {scanned} public signatures, which means it stopped \
         reading rather than that the rule holds"
    );
    found
}

/// Every handle-named type `cove-runtime` declares `pub`.
///
/// This is a category rather than a list of exceptions: a `pub` handle type
/// of this crate is not a private representation, because the private ones
/// are `pub(crate)` — which is what
/// [`the_only_public_handle_type_is_the_host_s`] keeps true. The other
/// category, a handle the standard library owns, is not one of ADR 0028's
/// five representations because all five are types this workspace defines.
fn crate_public_handles() -> BTreeSet<String> {
    sources()
        .iter()
        .flat_map(|path| public_handle_declarations(&read(path)))
        .collect()
}

/// Whether `signature` names a handle into storage the VM owns.
///
/// Every handle-named identifier in it must be a permitted type, or a
/// lowercase binding — `handle`, `handles` — in a signature where a
/// permitted type also appears, since that is what the binding is bound to.
/// A signature naming two handles, one permitted and one not, passes this;
/// the unpermitted one would have to be a private type in a public
/// signature, which the compiler refuses on its own.
fn names_a_vm_handle(signature: &str, permitted: &BTreeSet<String>) -> bool {
    let named: Vec<String> = identifiers(signature)
        .into_iter()
        .filter(|name| name.to_ascii_lowercase().contains(HANDLE))
        .collect();
    if named.is_empty() {
        return false;
    }
    let has_permitted_type = named.iter().any(|name| permitted.contains(name));
    !named.iter().all(|name| {
        permitted.contains(name)
            || (has_permitted_type && name.chars().all(|c| !c.is_ascii_uppercase()))
    })
}

/// Every handle-named type `text` declares as `pub` — and not `pub(crate)`,
/// which is what a VM heap handle is.
fn public_handle_declarations(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("pub ") else {
            continue;
        };
        for keyword in ["struct ", "enum ", "trait ", "type "] {
            let Some(name) = rest.strip_prefix(keyword) else {
                continue;
            };
            let name = name
                .split(['(', '<', '{', ' ', ';', '='])
                .next()
                .unwrap_or_default()
                .trim();
            if name.to_ascii_lowercase().contains(HANDLE) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Every handle-named type `text` imports from the standard library.
fn std_handle_types(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let Some(path) = line.trim().strip_prefix("use std::") else {
            continue;
        };
        for name in path.trim_end_matches(';').split(['{', '}', ',', ':', ' ']) {
            let name = name.trim();
            if !name.is_empty() && name.to_ascii_lowercase().contains(HANDLE) {
                out.insert(name.to_string());
            }
        }
    }
    out
}

/// The identifiers of a flattened signature.
fn identifiers(signature: &str) -> Vec<String> {
    signature
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every public signature in `text`, with its whitespace flattened: each
/// `pub fn`, and each method a `pub trait` declares.
///
/// The trait half is not in ADR 0028's wording — it says "a `grep` over `pub
/// fn`" — and ADR 0031 puts it in the rule, because a trait method is a
/// public signature that never carries the word `pub`.
/// `Callable::call_value` and `HostApi::call_resource` are exactly that
/// shape, so a grep for `pub fn` alone would let the next one through and
/// still pass.
///
/// `pub(crate) fn` is not a public signature and is skipped, which is what
/// lets the collector keep its own vocabulary: `heap::SlotRoots` is a
/// *binding* slot rather than one of ADR 0028's, and it is crate-visible, so
/// the rule has nothing to say about it and neither does this.
fn signatures(text: &str) -> Vec<String> {
    // Everything from here on is compiled only under `cfg(test)`, and a test
    // is not public API.
    let text = match text.find("\n#[cfg(test)]\nmod tests {") {
        Some(end) => &text[..end],
        None => text,
    };
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while let Some(start) = text[at..].find("pub fn ") {
        let start = at + start;
        at = start + "pub fn ".len();
        // `pub fn` and not `pub(crate) fn`, and not the tail of an
        // identifier or a doc comment's prose.
        let before = text[..start].rfind('\n').map(|i| &text[i + 1..start]);
        if !matches!(before, Some(indent) if indent.trim().is_empty()) {
            continue;
        }
        let end = bytes[start..]
            .iter()
            .position(|b| *b == b'{' || *b == b';')
            .map(|i| start + i);
        let Some(end) = end else { continue };
        out.push(flattened(&text[start..end]));
    }
    out.extend(trait_methods(text));
    out
}

/// Every method a `pub trait` in `text` declares, named `Trait::method` so
/// that a failure can be read without going to look it up.
fn trait_methods(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = text[at..].find("\npub trait ") {
        let start = at + found + 1;
        let Some(open) = text[start..].find('{').map(|i| start + i) else {
            break;
        };
        let name = text[start + "pub trait ".len()..open]
            .split([':', '<', ' '])
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let end = block_end(text, open);
        for (offset, _) in text[open..end].match_indices("\n    fn ") {
            let from = open + offset + 1;
            let Some(stop) = text[from..].find(['{', ';']).map(|i| from + i) else {
                continue;
            };
            let signature = flattened(&text[from..stop]);
            let signature = signature.strip_prefix("fn ").unwrap_or(&signature);
            out.push(format!("{name}::{signature}"));
        }
        at = end;
    }
    out
}

/// The index just past the `}` matching the `{` at `open`.
fn block_end(text: &str, open: usize) -> usize {
    let mut depth = 0;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + i + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

fn flattened(signature: &str) -> String {
    signature.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("a source file this crate compiles")
}

/// Every file of `cove-runtime` that is part of its public surface, which is
/// every file `lib.rs` does not declare private.
///
/// There is no path exclusion here and there was one until the cutover: the
/// deleted backend's `src/vm/tests/` directory, which the `src/vm/` of today
/// has no counterpart to. Nothing replaced it,
/// because nothing needs to. The rule this file runs is about what a `pub fn`
/// or a `pub trait` method may name, a module `lib.rs` declares with a bare
/// `mod` publishes nothing whatever its items say, and `private_modules`
/// reads exactly that. A `#[cfg(test)]` submodule of a private module is
/// covered twice over.
fn sources() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let private = private_modules(&root.join("lib.rs"));
    let mut out = Vec::new();
    walk(&root, &mut out);
    out.retain(|path| {
        !private
            .iter()
            .any(|name| path.file_stem().is_some_and(|stem| stem == name.as_str()))
    });
    out.sort();
    assert!(
        out.len() > 10,
        "the crate has more files than this; the walk is wrong"
    );
    out
}

/// The modules `lib.rs` declares without `pub`, which no embedder can reach.
fn private_modules(lib: &Path) -> Vec<String> {
    fs::read_to_string(lib)
        .expect("the crate root")
        .lines()
        .filter_map(|line| line.strip_prefix("mod ")?.strip_suffix(';'))
        .map(str::to_string)
        .collect()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("a directory of this crate") {
        let path = entry.expect("a readable entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}
