//! The Rust formatter's phases over the same corpus `examples/covefmt` walks.
//!
//! [Issue #369](https://github.com/myuon/cove/issues/369) asks for total wall
//! time, lex, parse and print for **three** arms — Rust, the encoded VM and the
//! template native tier — and the two Cove arms already report all four:
//! `examples/covefmt/bench.cove` times its three phases by difference and
//! prints them. The Rust arm had no such report, and the column could not be
//! filled from `cove fmt --check`, which is one wall-clock number over lexing,
//! parsing, formatting and comparing together.
//!
//! So this is the Rust arm's phase table, and it is a bin of its own rather
//! than a flag on `cove fmt`: it is a measurement with no baseline and no gate,
//! and `cove fmt` is a command a user runs. Adding `--timings` there would put a benchmark's output in a
//! formatter's interface for the sake of one table.
//!
//! # It is timed the same way the Cove bench is timed, and that is the point
//!
//! `bench.cove` measures a phase **by difference between two whole passes**,
//! and its own documentation says why at length: timing each phase over a table
//! of what the phase before it produced measures the table. The same trap is
//! here in a different shape — a `Vec<Vec<Token>>` of every file's tokens is
//! 82,872 tokens held live, which is an allocation profile no arm of the real
//! job has — so this does the same thing. Three passes over the sources, each
//! one a single loop with no intermediate kept across files:
//!
//! - pass 1 lexes;
//! - pass 2 lexes and parses;
//! - pass 3 lexes, parses, formats and compares, which is `cove fmt --check`'s
//!   whole job.
//!
//! Each phase is the difference between two of them, and pass 3 is the figure
//! to compare against the Cove arms' `whole`.
//!
//! One thing is deliberately **not** the same, and it is the honest asymmetry
//! between the arms: the Cove bench reads all 248 files into memory before it
//! starts its clock, and `cove fmt --check` reads each file inside its own. So
//! this reports the read as a phase of its own, apart from the three, and a
//! reader comparing `whole` with `whole` can see exactly how much is not
//! formatting.
//!
//! # Usage
//!
//! ```console
//! $ cargo build --profile checked -p cove-bench
//! $ ./target/checked/cove-fmt-phases            # the repository root
//! $ ./target/checked/cove-fmt-phases [root] [iterations]
//! ```
//!
//! It asserts the corpus it found, because a phase table over a different set
//! of files is not this table: `scripts/covefmt-tiers.sh` makes the same
//! assertion against the same two numbers, and issue #369's own figures were a
//! corpus two files and 14,017 bytes out of date.

use std::path::{Path, PathBuf};
use std::time::Instant;

use cove_diag::SourceMap;

/// Files the corpus holds. Asserted exactly: a phase table over 247 files is
/// not this table.
const FILES: usize = 248;

/// Bytes it holds, and the width of the band around it that still counts as the
/// same corpus.
///
/// A band and not an equality, where the file count above is an equality, and
/// the asymmetry is deliberate: **the corpus is this repository**, so editing a
/// doc comment anywhere in the tree moves this number. Correcting the stale
/// corpus figures that this check exists to catch moved it by 950 bytes on its
/// own. An exact assertion would be a tripwire on every prose change, which is
/// a gate nobody would keep; 2% is loose enough for prose and tight enough that
/// a corpus which grew enough to invalidate a timing cannot pass.
const BYTES: (usize, f64) = (698_481, 0.02);

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(args.first().map(String::as_str).unwrap_or("."));
    let iterations: usize = match args.get(1).map(|n| n.parse()) {
        Some(Ok(n)) => n,
        Some(Err(_)) => {
            eprintln!("usage: cove-fmt-phases [root] [iterations]");
            return std::process::ExitCode::FAILURE;
        }
        None => 9,
    };

    // The walk is a phase too, and a surprisingly expensive one: `cove fmt`
    // does it before it reads anything, and it is part of the difference
    // between this bin's `whole` and that command's process wall time.
    let started = Instant::now();
    let mut paths = Vec::new();
    walk(&root, &mut paths);
    paths.sort();
    let walked = started.elapsed();

    // The read is a phase, timed once, because it is the one part of
    // `cove fmt --check` that the Cove arms do before their clock starts.
    let started = Instant::now();
    let mut sources = Vec::with_capacity(paths.len());
    let mut bytes = 0usize;
    for path in &paths {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("cove-fmt-phases: cannot read `{}`: {error}", path.display());
                return std::process::ExitCode::FAILURE;
            }
        };
        bytes += text.len();
        sources.push((path.clone(), text));
    }
    let read = started.elapsed();

    let drift = (bytes as f64 - BYTES.0 as f64).abs() / BYTES.0 as f64;
    if sources.len() != FILES || drift > BYTES.1 {
        eprintln!(
            "cove-fmt-phases: the corpus under `{}` is {} files / {bytes} bytes, and this \
             measurement is published against {FILES} files / about {}. Update the recorded \
             facts in examples/covefmt/README.md, scripts/covefmt-tiers.sh and this bin \
             together, or explain the difference -- do not average over two corpora.",
            root.display(),
            sources.len(),
            BYTES.0,
        );
        return std::process::ExitCode::FAILURE;
    }

    // One `SourceMap` holding every file, built once: `lex` takes a map and a
    // `FileId`, so rebuilding one per pass would charge each pass a copy of
    // 697,531 bytes -- the very mistake the module documentation is about.
    let mut map = SourceMap::new();
    let files: Vec<_> = sources
        .iter()
        .map(|(path, text)| map.add(path.clone(), text.clone()))
        .collect();

    let mut rows: Vec<[f64; 3]> = Vec::with_capacity(iterations);
    let mut tokens = 0usize;
    let mut items = 0usize;
    let mut printed = 0usize;
    let mut unchanged = 0usize;
    let mut parsed = 0usize;
    for _ in 0..iterations {
        tokens = 0;
        let started = Instant::now();
        for &file in &files {
            if let Ok(held) = cove_syntax::lexer::lex(&map, file) {
                tokens += held.len();
            }
        }
        let lex = started.elapsed().as_secs_f64() * 1000.0;

        items = 0;
        let started = Instant::now();
        for &file in &files {
            if let Ok(held) = cove_syntax::lexer::lex(&map, file) {
                if let Ok(unit) = cove_syntax::parser::parse(&map, file, held) {
                    items += unit.items.len();
                }
            }
        }
        let lex_and_parse = started.elapsed().as_secs_f64() * 1000.0;

        // `cove fmt --check`'s whole job, less the read: parse (which numbers
        // the unit, and `format_source` needs the numbering), format, compare.
        // A file that does not parse is skipped and left alone, which is what
        // the command does -- three files under `tests/e2e` are written not to
        // parse, and `examples/covefmt/README.md` says why that matters.
        printed = 0;
        unchanged = 0;
        parsed = 0;
        let started = Instant::now();
        for (&file, (_, text)) in files.iter().zip(&sources) {
            let Ok(unit) = cove_syntax::parse_file(&map, file) else {
                continue;
            };
            parsed += 1;
            let formatted = cove_syntax::format::format_source(text, &unit);
            printed += formatted.len();
            if formatted == *text {
                unchanged += 1;
            }
        }
        let whole = started.elapsed().as_secs_f64() * 1000.0;
        rows.push([lex, lex_and_parse, whole]);
    }

    // Every file in this repository is `cove fmt`'s own output, so every file
    // that parses must come back unchanged. This is the same claim
    // `cove fmt --check` makes by exiting zero, made here so that a timing run
    // cannot be a timing run over a broken formatter.
    //
    // `parsed` is counted rather than written down as "all but the three under
    // `tests/e2e` that are written not to parse". A constant would turn a fourth
    // such file -- which is a reasonable thing for somebody to add -- into a
    // failure of the formatter, reported in the formatter's words.
    if unchanged < parsed {
        eprintln!(
            "cove-fmt-phases: {unchanged} of {parsed} parseable file(s) came back \
             unchanged, and every one of them is `cove fmt`'s own output"
        );
        return std::process::ExitCode::FAILURE;
    }

    let quantile = |pick: fn(&[f64; 3]) -> f64| {
        let mut held: Vec<f64> = rows.iter().map(pick).collect();
        held.sort_by(f64::total_cmp);
        (held[held.len() / 2], held[0], held[held.len() - 1])
    };
    let lex = quantile(|row| row[0]);
    let parse = quantile(|row| row[1] - row[0]);
    let print = quantile(|row| row[2] - row[1]);
    let whole = quantile(|row| row[2]);

    println!(
        "corpus  {} file(s), {bytes} byte(s), {tokens} token(s), {items} top-level item(s)",
        sources.len()
    );
    println!(
        "walk    {:.1} ms, once, and neither does the Cove bench",
        walked.as_secs_f64() * 1000.0
    );
    println!(
        "read    {:.1} ms, once, and no arm of the Cove bench pays it",
        read.as_secs_f64() * 1000.0
    );
    let say = |name: &str, (median, min, max): (f64, f64, f64), note: &str| {
        println!("{name:<7} {median:6.1} ms  [{min:.1}..{max:.1}]  {note}");
    };
    say("lex", lex, "cove_syntax::lexer::lex");
    say(
        "parse",
        parse,
        "cove_syntax::parser::parse, and the numbering",
    );
    say("print", print, "format_source, and the comparison");
    say(
        "whole",
        whole,
        "which is what `cove fmt --check` does, less the read",
    );
    println!(
        "{printed} byte(s) printed, {unchanged} of {parsed} file(s) that parse came \
         back unchanged; {} do not parse and are skipped, as `cove fmt` skips them",
        sources.len() - parsed,
    );
    std::process::ExitCode::SUCCESS
}

/// Every `.cove` file under `at`, by the same rule `examples/covefmt`'s `walk`
/// uses: a directory whose name holds a `.` is skipped, and so is `target`.
///
/// The two walks have to agree or the arms are not over the same bytes, which
/// `examples/covefmt/README.md` says was checked rather than assumed. The
/// corpus assertion in `main` is what keeps them agreeing.
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
