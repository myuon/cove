//! ADR 0028 decision 0's rule, as a test instead of a discipline.
//!
//! The ADR names five representations and makes exactly one of them public.
//! The sentence that makes issue #197's thesis — "changing the VM's internal
//! representation must not require exposing that representation to
//! embedders" — true by construction rather than by care is its visibility
//! column:
//!
//! > No public signature in this workspace mentions a `Slot`, a
//! > `HeapObject`, a `Dynamic`, a layout id, a witness, a handle, a frame
//! > base, or a tag.
//!
//! and it adds that this "is checkable — it is a `grep` over `pub fn`". This
//! file is that grep, run by `cargo test`, because a rule nobody runs is a
//! rule that rots. None of the four private representations exists yet;
//! that is the point. When one is built, the first `pub fn` that hands a
//! piece of it to an embedder fails here rather than shipping.
//!
//! # What it reads, and why that is `cove-runtime` and not the workspace
//!
//! The rule says "this workspace", and this checks `cove-runtime`. The
//! reason is the visibility column it comes from: representations 2, 3 and 4
//! — `Slot`, `HeapObject`, `Dynamic` — are listed as *private to
//! `cove-runtime`*, and `Value` is listed as the one public thing. So
//! `cove-runtime`'s public surface is where the rule bites. `cove-ir` names
//! slots publicly and always has, deliberately: ADR 0019's "Slots, not
//! names" and ADR 0027's places are the *lowering's* vocabulary, decided
//! before this ADR and untouched by it, and a lowered program is not
//! something an embedder is handed a piece of.
//!
//! # The exception list is exact in both directions
//!
//! A signature that names one of the forbidden words is listed below with
//! the reason it is not a representation, and the test fails both when an
//! unlisted one appears *and* when a listed one disappears. That is the
//! convention `REGISTERED_REFUSALS` uses in the differential suite, and the
//! reason is the same: an exception that quietly stops applying is an
//! exception nobody re-reads.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The vocabulary of the four private representations, as ADR 0028 lists it.
///
/// Matched case-insensitively against the identifiers of a `pub fn`'s
/// signature, so `Slot` catches `SlotRoots`, `slot`, and `frame_slot` alike.
/// That is deliberately blunt: a name close enough to be caught is a name
/// close enough to want a sentence about why it is allowed.
const FORBIDDEN: &[&str] = &[
    "slot",
    "heapobject",
    "dynamic",
    "witness",
    "layout",
    "handle",
    "frame_base",
    "framebase",
    "tag",
];

/// Every `pub fn` in `cove-runtime` whose signature names one of the words
/// above, with the reason it is not one of the four private representations.
///
/// Six of the seven are `ResourceHandle`, and ADR 0013 is why: a resource handle is
/// a *host's* handle, the name of something the host owns, and "every field
/// of it is part of the name". It is not a VM heap handle. ADR 0028 decides
/// the same thing twice over — its `ValueView` sketch has
/// `Resource(&'a ResourceHandle)` in it — so the word in the rule and this
/// type are two different things that happen to share a noun.
const ALLOWED: &[&str] = &[
    // host.rs — a host mints a handle for a resource it owns.
    "pub fn new(module: &str, resource: &ResourceSchema, id: u64) -> Arc<ResourceHandle>",
    // host.rs — two handles name the same resource.
    "pub fn names_same(&self, other: &ResourceHandle) -> bool",
    // value.rs — issue #195's reader for a resource value.
    "pub fn resource(&self) -> Option<&ResourceHandle>",
    // value.rs — the constructor that mirrors it.
    "pub fn from_resource(handle: impl Into<Arc<ResourceHandle>>) -> Value",
    // host.rs — the operation a host answers *on* one of its own resources.
    "pub fn call_resource( &self, handle: &ResourceHandle, op: &str, args: Vec<Value>, back: &mut dyn Reentry, ) -> Result<Value, RuntimeError>",
    // host.rs — the same operation as a trait method, which is where a host
    // implements it. Caught only because this reads `pub trait` as well as
    // `pub fn`; ADR 0028's wording would have missed it.
    "HostApi::call_resource( &self, handle: &ResourceHandle, op: &str, args: Vec<Value>, back: &mut dyn Reentry, ) -> Result<Value, RuntimeError>",
    // task.rs — `std::thread::JoinHandle`, caught by the same blunt match.
    // ADR 0008 gives a spawned task a thread, and this is the standard
    // library's name for the thing that joins one; nothing about it is a
    // reference into a Cove heap.
    "pub fn running( id: u64, scope: Rc<str>, position: usize, cancellation: Cancellation, thread: JoinHandle<TaskOutcome>, ) -> Rc<Task>",
];

#[test]
fn no_public_signature_names_an_internal_representation() {
    let found = public_signatures_naming_a_representation();
    let allowed: BTreeSet<String> = ALLOWED.iter().map(|s| (*s).to_string()).collect();

    let unlisted: Vec<&String> = found.difference(&allowed).collect();
    assert!(
        unlisted.is_empty(),
        "a public signature in `cove-runtime` names an internal representation, which \
         ADR 0028 decision 0 forbids:\n  {}\n\nIf the name is not one of the four private \
         representations — the way `ResourceHandle` is not — add it to `ALLOWED` in this \
         file with the sentence that says why.",
        unlisted
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let stale: Vec<&String> = allowed.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "`ALLOWED` lists a signature that no longer exists, so its justification is no \
         longer read by anybody:\n  {}",
        stale
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// The signatures the rule is about: every `pub fn` in a public module of
/// `cove-runtime` that names one of [`FORBIDDEN`].
fn public_signatures_naming_a_representation() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for path in sources() {
        let text = fs::read_to_string(&path).expect("a source file this crate compiles");
        for signature in signatures(&text) {
            let words = signature.to_ascii_lowercase();
            if FORBIDDEN.iter().any(|word| words.contains(word)) {
                found.insert(signature);
            }
        }
    }
    assert!(
        !found.is_empty(),
        "the scan found nothing at all, which means it stopped reading rather than that \
         the rule holds"
    );
    found
}

/// Every public signature in `text`, with its whitespace flattened: each
/// `pub fn`, and each method a `pub trait` declares.
///
/// The trait half is not in ADR 0028's wording — it says "a `grep` over `pub
/// fn`" — and it is here because a trait method is a public signature that
/// never carries the word `pub`. `Callable::call_value` and
/// `HostApi::call_resource` are exactly that shape, so a grep for `pub fn`
/// alone would let the next one through and still pass.
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
/// that an exception can be read without going to look it up.
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

/// Every file of `cove-runtime` that is part of its public surface: the
/// private module `invoke` and the VM's test-only submodules are not.
fn sources() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let private = private_modules(&root.join("lib.rs"));
    let mut out = Vec::new();
    walk(&root, &mut out);
    out.retain(|path| {
        !path.starts_with(root.join("vm").join("tests"))
            && !private
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
