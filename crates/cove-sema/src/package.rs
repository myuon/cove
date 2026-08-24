//! Discovering and loading a Cove package from disk.
//!
//! A package is rooted at the nearest `cove.toml`. Module paths are relative to
//! that root: each directory containing `.cove` files is one module, and every
//! `.cove` file in it is an implementation unit of that module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cove_diag::{Diagnostic, FileId, SourceMap, Span};
use cove_syntax::ast::SourceUnit;

use crate::config::{self, Config};

/// One parsed `.cove` file.
#[derive(Debug)]
pub struct Unit {
    pub file: FileId,
    pub path: PathBuf,
    pub ast: SourceUnit,
}

/// One directory of `.cove` files.
#[derive(Debug)]
pub struct Module {
    /// Dotted name derived from the directory path, such as `hello` or
    /// `booking.create`.
    pub name: String,
    pub dir: PathBuf,
    pub units: Vec<Unit>,
}

/// A loaded package: its configuration and every module below its root.
#[derive(Debug)]
pub struct Package {
    pub root: PathBuf,
    pub config: Config,
    pub modules: BTreeMap<String, Module>,
}

/// Loads the package rooted at `root`, reading every source file it contains
/// into `sources`.
pub fn load(root: &Path, sources: &mut SourceMap) -> Result<Package, Vec<Diagnostic>> {
    let config_path = root.join("cove.toml");
    let text = std::fs::read_to_string(&config_path).map_err(|e| {
        vec![Diagnostic::error(
            "cove::package::config",
            format!("cannot read `{}`: {e}", config_path.display()),
        )]
    })?;
    let config =
        config::parse(&text).map_err(|e| vec![Diagnostic::error("cove::package::config", e)])?;

    let mut modules = BTreeMap::new();
    let mut diagnostics = Vec::new();
    walk(root, root, &mut modules, sources, &mut diagnostics);

    if diagnostics.is_empty() {
        Ok(Package {
            root: root.to_path_buf(),
            config,
            modules,
        })
    } else {
        Err(diagnostics)
    }
}

/// Recursively visits `dir`, turning every directory with `.cove` files
/// directly inside it into a module.
fn walk(
    root: &Path,
    dir: &Path,
    modules: &mut BTreeMap<String, Module>,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            diagnostics.push(Diagnostic::error(
                "cove::package::io",
                format!("cannot read `{}`: {e}", dir.display()),
            ));
            return;
        }
    };

    let mut names: Vec<std::ffi::OsString> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        names.push(entry.file_name());
    }
    names.sort();

    let mut cove_files = Vec::new();
    let mut subdirs = Vec::new();
    for name in names {
        let path = dir.join(&name);
        if path.is_dir() {
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') || name_str == "target" {
                continue;
            }
            subdirs.push(path);
        } else if path.extension().and_then(|e| e.to_str()) == Some("cove") {
            cove_files.push(path);
        }
    }

    if !cove_files.is_empty() {
        handle_module_dir(root, dir, &cove_files, modules, sources, diagnostics);
    }

    for subdir in subdirs {
        walk(root, &subdir, modules, sources, diagnostics);
    }
}

fn handle_module_dir(
    root: &Path,
    dir: &Path,
    cove_files: &[PathBuf],
    modules: &mut BTreeMap<String, Module>,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let rel = dir
        .strip_prefix(root)
        .expect("walk only visits descendants of root");

    if rel.as_os_str().is_empty() {
        for file in cove_files {
            let text = match std::fs::read_to_string(file) {
                Ok(text) => text,
                Err(e) => {
                    diagnostics.push(Diagnostic::error(
                        "cove::package::io",
                        format!("cannot read `{}`: {e}", file.display()),
                    ));
                    continue;
                }
            };
            let file_id = sources.add(file.clone(), text.clone());
            diagnostics.push(
                Diagnostic::error(
                    "cove::package::root_module",
                    format!(
                        "`{}` has no module: source must live in a directory",
                        file.display()
                    ),
                )
                .at(Span::new(file_id, 0, text.len() as u32))
                .rule(
                    "A directory is a module; `.cove` files directly in the package root are not.",
                )
                .help(format!(
                    "Move `{}` into a subdirectory such as `src/{}`.",
                    file.display(),
                    file.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )),
            );
        }
        return;
    }

    let components: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();

    if let Some(invalid) = components.iter().find(|c| !is_valid_identifier(c)) {
        diagnostics.push(
            Diagnostic::error(
                "cove::package::module_name",
                format!(
                    "`{invalid}` is not a valid module name component in `{}`",
                    dir.display()
                ),
            )
            .rule(
                "A module name is derived from its directory path and must be a valid Cove identifier.",
            )
            .help(format!(
                "Rename the `{invalid}` directory to match `[A-Za-z_][A-Za-z0-9_]*`."
            )),
        );
        return;
    }

    let name = components.join(".");
    let mut units = Vec::new();
    for file in cove_files {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) => {
                diagnostics.push(Diagnostic::error(
                    "cove::package::io",
                    format!("cannot read `{}`: {e}", file.display()),
                ));
                continue;
            }
        };
        let file_id = sources.add(file.clone(), text);
        match cove_syntax::parse_file(sources, file_id) {
            Ok(ast) => units.push(Unit {
                file: file_id,
                path: file.clone(),
                ast,
            }),
            Err(errs) => diagnostics.extend(errs),
        }
    }

    modules.insert(
        name.clone(),
        Module {
            name,
            dir: dir.to_path_buf(),
            units,
        },
    );
}

/// Whether `s` matches `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cove-sema-test-{name}-{}-{}",
                std::process::id(),
                nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
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

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const FN_MAIN: &str = "export fn main() -> Result<Unit, Error> {\n  Ok(())\n}\n";

    #[test]
    fn discovers_one_module_per_directory() {
        let dir = TempDir::new("modules");
        write(
            dir.path(),
            "cove.toml",
            "[run.hello]\nentry = \"hello.main\"\n",
        );
        write(dir.path(), "hello/main.cove", FN_MAIN);
        write(dir.path(), "src/booking/create.cove", FN_MAIN);

        let mut sources = SourceMap::new();
        let package = load(dir.path(), &mut sources).expect("loads");
        let mut names: Vec<&String> = package.modules.keys().collect();
        names.sort();
        assert_eq!(names, vec!["hello", "src.booking"]);
    }

    #[test]
    fn skips_hidden_and_target_directories() {
        let dir = TempDir::new("skips");
        write(
            dir.path(),
            "cove.toml",
            "[run.hello]\nentry = \"hello.main\"\n",
        );
        write(dir.path(), "hello/main.cove", FN_MAIN);
        write(dir.path(), ".git/stray.cove", FN_MAIN);
        write(dir.path(), "target/stray.cove", FN_MAIN);
        write(dir.path(), "hello/target/stray.cove", FN_MAIN);

        let mut sources = SourceMap::new();
        let package = load(dir.path(), &mut sources).expect("loads");
        assert_eq!(package.modules.len(), 1);
        assert!(package.modules.contains_key("hello"));
    }

    #[test]
    fn rejects_cove_file_directly_in_root() {
        let dir = TempDir::new("root-module");
        write(
            dir.path(),
            "cove.toml",
            "[run.hello]\nentry = \"hello.main\"\n",
        );
        write(dir.path(), "main.cove", FN_MAIN);

        let mut sources = SourceMap::new();
        let errs = load(dir.path(), &mut sources).unwrap_err();
        assert!(errs.iter().any(|d| d.code == "cove::package::root_module"));
    }

    #[test]
    fn rejects_invalid_module_name() {
        let dir = TempDir::new("bad-name");
        write(
            dir.path(),
            "cove.toml",
            "[run.hello]\nentry = \"hello.main\"\n",
        );
        write(dir.path(), "not-an-ident/main.cove", FN_MAIN);

        let mut sources = SourceMap::new();
        let errs = load(dir.path(), &mut sources).unwrap_err();
        assert!(errs.iter().any(|d| d.code == "cove::package::module_name"));
    }

    #[test]
    fn missing_config_produces_a_diagnostic() {
        let dir = TempDir::new("no-config");
        let mut sources = SourceMap::new();
        let errs = load(dir.path(), &mut sources).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, "cove::package::config");
    }

    #[test]
    fn loads_the_real_examples_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut sources = SourceMap::new();
        let package = load(&root, &mut sources).expect("examples package loads");
        assert!(package.modules.contains_key("hello"));
        assert!(package.modules.contains_key("server"));
    }
}
