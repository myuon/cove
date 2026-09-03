//! Fixture packages for the CLI's own tests.
//!
//! A test package is written to a real temporary directory, so relative
//! paths, `SourceMap::path`, and the package walk behave exactly as they do
//! for a package the CLI loads from disk.

use std::path::{Path, PathBuf};

use cove_diag::SourceMap;
use cove_sema::package::Package;
use cove_sema::resolve::Program;

/// A temporary directory that removes itself when the test ends.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cove-cli-test-{name}-{}-{}",
            std::process::id(),
            nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Writes `text` to `dir/rel`, creating the directories it needs.
pub(crate) fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Loads and resolves the package rooted at `root`.
///
/// Resolution is enough for a test about what the resolver derives — a
/// public interface, a call graph, an outline — and several fixtures below
/// this one are written loosely enough that they resolve and do not check.
pub(crate) fn load_fixture(root: &Path) -> (SourceMap, Package, Program) {
    let mut sources = SourceMap::new();
    let package = cove_sema::package::load(root, &mut sources).expect("fixture package loads");
    let program = cove_sema::resolve::resolve(&package).expect("fixture package resolves");
    (sources, package, program)
}

/// Loads and *checks* the package rooted at `root`, exactly as
/// [`crate::load`] does for a package on disk.
///
/// Which is what a fixture that will be executed needs, and what
/// [`load_fixture`] is not. The difference never showed while the
/// interpreter was the only backend, because a tree walk reads no type it
/// was not handed. `cove_lir::lower` reads several — ADR 0019 makes the IR a
/// recording of the checker's answers rather than a second derivation of
/// them — so a fixture that skipped the checker made the lowering refuse a
/// program the real CLI lowers without complaint, which is a fixture
/// reporting on itself.
pub(crate) fn check_fixture(root: &Path) -> (SourceMap, Package, Program) {
    let mut sources = SourceMap::new();
    let package = cove_sema::package::load(root, &mut sources).expect("fixture package loads");
    let program = cove_sema::Compiler::new()
        .compile(&package)
        .expect("fixture package checks");
    (sources, package, program)
}

/// The real `examples/` package at the repository root.
pub(crate) fn examples_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}
