//! The fingerprint `cove_ir::serial` stamps on a program image.
//!
//! [ADR 0077](../../docs/adr/0077-cove-fmt-is-covefmt.md) makes a serialized
//! program readable only by the build that wrote it, and this is how "the
//! build" is named: a 64-bit FNV-1a hash of every `.rs` file under `src/`,
//! path and contents, in sorted order, together with the crate's version.
//!
//! It is derived rather than bumped by hand because the format is the IR's
//! own types written out field by field, and the opcode numbers it carries
//! are, in `bytecode::op`'s words, positions in a generated table that move
//! when the table does. A constant somebody has to remember to change when
//! any of that changes is a constant somebody forgets to change; a hash of
//! the source cannot be forgotten. What it costs is that an edit that changes
//! nothing about the format — a comment, a lowering pass — changes the
//! fingerprint too, and that costs nothing, because the only image there is
//! is rebuilt in the same build.
//!
//! The source is hashed and not the compiled crate, because the build script
//! of `cove-cli` compiles this crate for the *host* and the `cove` binary
//! compiles it for the *target*: two different artifacts from one source,
//! which must agree. Nothing in this crate is conditional on the target (a
//! test in `serial` holds that), so one source is one format.

use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let src = root.join("src");
    println!("cargo:rerun-if-changed=src");

    let mut files = Vec::new();
    collect(&src, &mut files);
    files.sort();

    let mut hash = Fnv::new();
    hash.write(env!("CARGO_PKG_VERSION").as_bytes());
    for path in &files {
        let relative = path.strip_prefix(&root).unwrap_or(path);
        // Separators normalised, so that the same tree hashes alike on every
        // host that might build it.
        hash.write(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hash.write(&[0]);
        let bytes = std::fs::read(path).expect("a source file this build compiles");
        hash.write(&(bytes.len() as u64).to_le_bytes());
        hash.write(&bytes);
    }

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo"));
    std::fs::write(
        out.join("fingerprint.rs"),
        format!(
            "/// See `build.rs`: a hash of this crate's source, written by its build script.\n\
             pub const FINGERPRINT: u64 = {:#018x};\n",
            hash.finish()
        ),
    )
    .expect("OUT_DIR is writable");
}

fn collect(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, found);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            found.push(path);
        }
    }
}

/// 64-bit FNV-1a: no dependency, and a fingerprint has no adversary.
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
