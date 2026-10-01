//! Lowers `tools/covefmt` into the image `cove fmt` runs.
//!
//! [ADR 0077](../../docs/adr/0077-cove-fmt-is-covefmt.md): `cove fmt` is
//! covefmt, the formatter written in Cove, and the `cove` binary carries it as
//! lowered IR so that formatting a file pays no front end and no lowering. This
//! is where that IR is made — with the same front end and lowering `cove run`
//! uses, run once, when the toolchain is built:
//!
//! 1. load `tools/covefmt` as a package and check it against the standard
//!    library, exactly as `cove check` there would;
//! 2. lower what `covefmt.formatSource` reaches, exactly as `cove run` would;
//! 3. refuse a lowering that reaches a host operation: `cove fmt` runs covefmt
//!    with no capability at all, and a formatter that wanted one would be
//!    refused at its first call rather than at build time;
//! 4. write the image `cove_ir::serial::encode` makes to `OUT_DIR`, where
//!    `src/covefmt.rs` embeds it.
//!
//! A covefmt that does not check or lower **fails the build**, with its
//! diagnostics, so a `cove` binary never carries a formatter it could not have
//! run. Nothing generated is checked in.
//!
//! The front end and the lowering run optimised, through the
//! `[profile.*.build-override]` tables in the workspace manifest: unoptimised,
//! lowering covefmt alone would cost every build of this crate seconds.

use std::path::PathBuf;
use std::time::Instant;

/// The function `cove fmt` calls once per file.
const ENTRY: (&str, &str) = ("covefmt", "formatSource");

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let package = manifest.join("../../tools/covefmt");
    let std = manifest.join("../cove-sema/std");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", package.display());
    println!("cargo:rerun-if-changed={}", std.display());

    let started = Instant::now();
    let mut sources = cove_diag::SourceMap::new();
    let loaded = cove_sema::package::load(&package, &mut sources);
    let program = loaded.and_then(|package| cove_sema::Compiler::new().compile(&package));
    let program = match program {
        Ok(program) => program,
        Err(items) => fail(&sources, "does not check", &items),
    };
    let checked = started.elapsed();

    let started = Instant::now();
    let lowered = cove_ir::lower_entry(
        &program,
        &sources,
        &cove_sema::HostSchemas::new(),
        ENTRY.0,
        ENTRY.1,
    );
    let lowered = match lowered {
        Ok(lowered) => lowered,
        Err(items) => fail(&sources, "does not lower", &items),
    };
    let lower = started.elapsed();
    if let Some(op) = lowered.host_ops.first() {
        eprintln!(
            "error: `{}.{}` reaches the host operation `{}`, and `cove fmt` runs covefmt with \
             no capability at all (ADR 0077)",
            ENTRY.0,
            ENTRY.1,
            op.qualified()
        );
        std::process::exit(1);
    }

    let started = Instant::now();
    let image =
        cove_ir::serial::encode(&lowered, &sources, ENTRY.0, ENTRY.1).unwrap_or_else(|why| {
            eprintln!("error: covefmt's lowering cannot be written as an image: {why}");
            std::process::exit(1);
        });
    let encode = started.elapsed();

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo"));
    std::fs::write(out.join("covefmt.ir"), &image).expect("OUT_DIR is writable");
    // What this run cost and made, for a reader measuring it: ADR 0077's
    // Measurement section was filled from this file.
    let functions = lowered.functions.len();
    let lowered_bodies = lowered.functions.iter().filter(|f| !f.stub).count();
    let instructions: usize = lowered.functions.iter().map(|f| f.code.len()).sum();
    let read = cove_ir::serial::decode(&image).unwrap_or_else(|why| {
        eprintln!("error: the image this build just wrote does not read back: {why}");
        std::process::exit(1);
    });
    let carried: usize = read
        .sources
        .files()
        .filter(|file| !read.sources.is_library(file.id))
        .map(|file| file.text.len())
        .sum();
    std::fs::write(
        out.join("covefmt.ir.txt"),
        format!(
            "image {} bytes, {carried} of them covefmt's own source text\n\
             functions {functions} ({lowered_bodies} lowered, the rest stubs)\n\
             instructions {instructions}\ncheck {:.1} ms\nlower {:.1} ms\nencode {:.1} ms\n",
            image.len(),
            checked.as_secs_f64() * 1e3,
            lower.as_secs_f64() * 1e3,
            encode.as_secs_f64() * 1e3,
        ),
    )
    .expect("OUT_DIR is writable");
}

/// Stops the build with covefmt's own diagnostics.
fn fail(sources: &cove_diag::SourceMap, what: &str, items: &[cove_diag::Diagnostic]) -> ! {
    for item in items {
        eprint!("{}", cove_diag::render(sources, item));
    }
    eprintln!(
        "error: tools/covefmt {what}, and `cove fmt` is covefmt (ADR 0077): the `cove` binary \
         cannot be built without it"
    );
    std::process::exit(1);
}
