//! The formatter `cove fmt` runs: `tools/covefmt`, carried as lowered IR.
//!
//! [ADR 0077](../../../docs/adr/0077-cove-fmt-is-covefmt.md) decides it.
//! `build.rs` checks `tools/covefmt`, lowers `covefmt.formatSource`, and writes
//! the result with `cove_ir::serial`; this module embeds those bytes, reads them
//! back once per `cove fmt`, and calls `formatSource` once per file. Nothing
//! here parses, checks or lowers Cove: the front end ran when the toolchain was
//! built.
//!
//! covefmt runs with **no capability**. Its host registry has no module in it
//! and grants nothing, and `build.rs` refuses a lowering that reaches a host
//! operation at all, so the formatter cannot read, write or reach anything; it
//! is handed a `String` and answers a `Formatted`.
//!
//! It runs on the native tier where this build and this host have one (ADR
//! 0076: x86-64 Unix, default build) and on the encoded VM otherwise. That is
//! not the silent substitution ADR 0055 forbids — the two tiers run one
//! lowering under one runtime, and CI asserts they print the same bytes over
//! the whole corpus — and `--backend vm` and `--backend native` choose one
//! explicitly.

use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::host::{Grants, HostRegistry};
use cove_runtime::{NativeProgram, Runtime, RuntimeError, Value, Vm};

use crate::Backend;

/// The image `build.rs` wrote.
static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/covefmt.ir"));

/// How many bytes the embedded image is.
#[cfg(test)]
pub(crate) fn image_bytes() -> usize {
    IMAGE.len()
}

/// What covefmt answered for one file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The file as covefmt formats it, which may be the file unchanged.
    Formatted(String),
    /// covefmt's own meaning check refused what it would have written, for
    /// this reason. The file is to be left alone: this is a formatter bug.
    Refused(String),
}

/// covefmt, read back and set up: the program, its runtime, and — where there
/// is one — its machine code.
///
/// Built once per `cove fmt`; [`Covefmt::session`] is what formats.
pub(crate) struct Covefmt {
    program: cove_ir::Program,
    module: Arc<str>,
    name: Arc<str>,
    runtime: Runtime,
    sources: Arc<SourceMap>,
    /// Compiled before the first run and dropped after the last, because it
    /// owns the pages its entries point into.
    native: Option<NativeProgram>,
}

impl Covefmt {
    /// Reads the image and sets covefmt up on the tier `backend` names, or on
    /// the native tier where available and the VM otherwise when it names
    /// none.
    ///
    /// `--backend native` on a host or build without the tier answers ADR
    /// 0055's capability diagnostic, as `cove run --backend native` does.
    /// `--backend ast` never reaches here: `cmd_fmt` refuses it first.
    pub(crate) fn load(backend: Option<Backend>) -> Result<Covefmt, String> {
        let image = cove_ir::serial::decode(IMAGE)
            .map_err(|why| format!("the formatter built into this `cove` cannot be read: {why}"))?;
        let native =
            match backend {
                Some(Backend::Vm) => None,
                Some(Backend::Native) => {
                    Some(cove_runtime::compile_native(&image.program).map_err(|e| e.to_string())?)
                }
                None => cove_runtime::compile_native(&image.program).ok(),
                Some(Backend::Ast) => return Err(
                    "`cove fmt` cannot run on `--backend ast`: covefmt is carried as lowered IR"
                        .to_string(),
                ),
            };
        let sources = Arc::new(image.sources);
        // No module, no grant: covefmt is pure, and a host call it made would
        // be refused at the call.
        let hosts = Arc::new(HostRegistry::new(Grants::default()));
        // The checked program a `Runtime` is built over is the front end's,
        // and there is none here; `Vm::invoke_lowered` is the way in that does
        // not read it.
        let runtime = Runtime::new(Arc::default(), Arc::clone(&sources), hosts);
        Ok(Covefmt {
            program: image.program,
            module: image.module,
            name: image.name,
            runtime,
            sources,
            native,
        })
    }

    /// Which tier this setup runs covefmt on.
    #[cfg(test)]
    pub(crate) fn tier(&self) -> Backend {
        match self.native {
            Some(_) => Backend::Native,
            None => Backend::Vm,
        }
    }

    /// The source map a runtime error from covefmt points into.
    pub(crate) fn sources(&self) -> &Arc<SourceMap> {
        &self.sources
    }

    /// One run of covefmt, invoked once per file.
    pub(crate) fn session(&self) -> Session<'_> {
        let vm = match &self.native {
            Some(native) => {
                Vm::with_native(&self.runtime, self.runtime.hosts(), &self.program, native)
            }
            None => Vm::new(&self.runtime, self.runtime.hosts(), &self.program),
        };
        Session {
            vm,
            module: &self.module,
            name: &self.name,
        }
    }
}

/// A run of covefmt that formats one file per call.
pub(crate) struct Session<'a> {
    vm: Vm<'a>,
    module: &'a str,
    name: &'a str,
}

impl Session<'_> {
    /// `source` as covefmt formats it, or the reason it refused to.
    pub(crate) fn format(&mut self, source: &str) -> Result<Answer, RuntimeError> {
        let answer = self
            .vm
            .invoke_lowered(self.module, self.name, vec![Value::string(source)])?;
        let text = answer.field("text").and_then(Value::as_str);
        let refused = answer.field("refused").map(|held| {
            held.some_payload()
                .and_then(|payload| payload.first())
                .and_then(Value::as_str)
        });
        match (text, refused) {
            (Some(_), Some(Some(reason))) => Ok(Answer::Refused(reason.to_string())),
            (Some(text), Some(None)) => Ok(Answer::Formatted(text.to_string())),
            _ => Err(RuntimeError::new(format!(
                "`{}.{}` answered something that is not a `Formatted`: {}",
                self.module,
                self.name,
                answer.type_name()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use super::*;

    /// Every `.cove` file under `at`, by the rule `covefmtBench` walks with: a
    /// directory whose name holds a `.` is skipped, and so is `target`.
    fn walk(at: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".cove") {
                found.push(path);
            } else if !name.contains('.') && name != "target" {
                walk(&path, found);
            }
        }
    }

    /// One way of damaging a formatted file without making it unparseable:
    /// the formatter is asked to repair it, and the Rust formatter is asked
    /// the same, and the two answers are compared.
    ///
    /// None of these is judged against the file it came from — `covefmtBench`
    /// does that for its own four, whose repair is the file. These are judged
    /// against `cove_syntax::format` on the damaged text, which is what lets a
    /// damage be one no formatter could undo: a blank line taken out of a body
    /// is not put back, and both formatters must agree that it is not.
    type Damage = fn(&str) -> String;

    /// Every line's leading space taken out.
    fn deindented(text: &str) -> String {
        text.split_inclusive('\n').map(str::trim_start).collect()
    }

    /// Every blank line taken out — the one damage that asks where a blank
    /// line *must* go, between two declarations.
    fn without_blank_lines(text: &str) -> String {
        text.split_inclusive('\n')
            .filter(|line| !line.trim().is_empty())
            .collect()
    }

    /// The space just inside a brace taken out wherever a brace has some:
    /// `{ x }` written `{x}`, which asks for a space the source has not got.
    fn squeezed(text: &str) -> String {
        text.replace("{ ", "{").replace(" }", "}")
    }

    /// Three newlines at the end, and the indentation of a space at the top.
    fn loose_ends(text: &str) -> String {
        format!(" {}\n\n\n", text.trim_end())
    }

    /// All four at once: one pass over the corpus rather than four, because
    /// a rule that fails under any of them fails under all of them together,
    /// and the oracle is run on every `cargo t`.
    fn damaged(text: &str) -> String {
        loose_ends(&squeezed(&without_blank_lines(&deindented(text))))
    }

    /// **The oracle.** `cove fmt` is covefmt, and the Rust formatter it
    /// replaced stays as the judge of it: on every `.cove` file in the
    /// repository that parses, and on a damaged version of each, the
    /// formatter built into this binary answers exactly what
    /// `cove_syntax::format` answers, and refuses none.
    ///
    /// ADR 0077's decision 5. It is a judge the formatter does not supply —
    /// issue 402's lesson, that a test which borrows the subject's own helper
    /// is blind — and it is what keeps `cove generate` and the AST's
    /// `Display`, which still format with the Rust formatter, from drifting
    /// from `cove fmt`. Every file here passes `cove fmt --check`, so each one
    /// is covefmt's fixed point; the undamaged half of this makes it the Rust
    /// formatter's too, which is what lets `covefmtBench`'s mutations, judged
    /// against the file on disk, go on being judged against the Rust
    /// formatter.
    ///
    /// The damaged half is the one that found something. With only the
    /// undamaged files, covefmt agreed with the Rust formatter everywhere —
    /// and wrote two declarations with no blank line between them, a body
    /// against its braces as `\{ 1\n \}`, and a comment with the space it
    /// ended on, on input any editor produces. Each of those is a damage
    /// below.
    ///
    /// One damage is left out because it does not pass yet: doubling every
    /// space, string literals included, pushes lines past the width, and on
    /// two of them — a call around a call around a long literal — covefmt
    /// breaks the outer call where the Rust formatter hugs it. That is the
    /// next thing to fix in covefmt, and it is named here rather than
    /// asserted.
    ///
    /// One setup and one session for the whole walk, as `cove fmt` does it,
    /// on the native tier where there is one.
    #[test]
    fn covefmt_formats_every_file_in_the_repository_as_the_rust_formatter_does() {
        let damages: [(&str, Damage); 2] = [
            ("as it is", |text| text.to_string()),
            (
                "deindented, without blank lines, squeezed and with loose ends",
                damaged,
            ),
        ];
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut paths = Vec::new();
        walk(&root, &mut paths);
        paths.sort();

        let started = Instant::now();
        let covefmt = Covefmt::load(None).expect("the built-in formatter loads");
        let setup = started.elapsed();
        let mut session = covefmt.session();
        let mut sources = SourceMap::new();
        let (mut compared, mut unparsed) = (vec![0; damages.len()], 0);
        let mut faults = Vec::new();
        let mut spent = std::time::Duration::ZERO;
        let walked = Instant::now();
        for path in &paths {
            let original = std::fs::read_to_string(path).unwrap();
            let shown = path.strip_prefix(&root).unwrap_or(path).display();
            for (at, (damage, damaged)) in damages.iter().enumerate() {
                let text = damaged(&original);
                let file = sources.add(path, text.clone());
                let Ok(unit) = cove_syntax::parse_file(&sources, file) else {
                    if at == 0 {
                        unparsed += 1;
                    }
                    continue;
                };
                let expected = cove_syntax::format::format_source(&text, &unit);
                let started = Instant::now();
                let answer = session.format(&text);
                if at == 0 {
                    spent += started.elapsed();
                }
                compared[at] += 1;
                match answer {
                    Ok(Answer::Formatted(found)) if found == expected => {}
                    Ok(Answer::Formatted(found)) => {
                        let line = found
                            .lines()
                            .zip(expected.lines())
                            .position(|(a, b)| a != b)
                            .unwrap_or(found.lines().count().min(expected.lines().count()))
                            + 1;
                        faults.push(format!(
                            "{shown} ({damage}): differs from the Rust formatter at line {line}"
                        ));
                    }
                    Ok(Answer::Refused(reason)) => {
                        faults.push(format!("{shown} ({damage}): refused: {reason}"))
                    }
                    Err(error) => {
                        faults.push(format!("{shown} ({damage}): failed: {}", error.message))
                    }
                }
            }
        }
        println!(
            "covefmt on {:?}: {:?} file(s) compared ({}), {unparsed} that do not parse \
             skipped; setup {:.1} ms, formatting the undamaged files {:.1} ms, the whole \
             test {:.1} ms",
            covefmt.tier(),
            compared,
            damages.map(|(name, _)| name).join(", "),
            setup.as_secs_f64() * 1e3,
            spent.as_secs_f64() * 1e3,
            walked.elapsed().as_secs_f64() * 1e3,
        );
        assert!(
            compared.iter().all(|n| *n > 300),
            "only {compared:?} files were compared; the walk has lost the repository"
        );
        assert!(
            faults.is_empty(),
            "covefmt does not answer what the Rust formatter answers on {} input(s):\n{}",
            faults.len(),
            faults.join("\n")
        );
    }

    /// The image is the one `build.rs` wrote, and it reads back.
    #[test]
    fn the_built_in_image_reads_back() {
        let image = cove_ir::serial::decode(IMAGE).expect("the embedded image reads back");
        assert_eq!((&*image.module, &*image.name), ("covefmt", "formatSource"));
        assert!(
            image.program.host_ops.is_empty(),
            "covefmt needs no capability"
        );
        assert!(image_bytes() > cove_ir::serial::HEADER_BYTES);
    }
}
