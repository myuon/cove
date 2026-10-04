//! `cove-edge-timeline`: draws a request timeline that `cove-edge
//! --timeline` recorded.
//!
//! ```text
//! cargo run --release -p cove-edge --bin cove-edge-timeline -- timeline.json \
//!     [-o timeline.html] [--perfetto timeline.perfetto.json] [--svg timeline.svg]
//! ```
//!
//! `-o` is a self-contained HTML page (one inline SVG, a hover readout, a
//! table view); `--perfetto` is Chrome Trace Event JSON for
//! <https://ui.perfetto.dev>; `--svg` is the figure alone, with its title,
//! summary and legend inside it, for an image. The summary is printed either
//! way.

use std::process::ExitCode;

use cove_edge::picture;

const USAGE: &str = "\
usage: cove-edge-timeline TIMELINE.json [-o OUT.html] [--perfetto OUT.json] [--svg OUT.svg]";

fn main() -> ExitCode {
    let mut input = None;
    let mut html = None;
    let mut perfetto = None;
    let mut svg = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| fail("a flag takes a value"));
        match arg.as_str() {
            "-o" | "--html" => html = Some(value()),
            "--perfetto" => perfetto = Some(value()),
            "--svg" => svg = Some(value()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            flag if flag.starts_with('-') => fail(&format!("unknown argument `{flag}`")),
            path => input = Some(path.to_string()),
        }
    }
    let input = input.unwrap_or_else(|| fail("name the timeline to draw"));
    let text = std::fs::read_to_string(&input)
        .unwrap_or_else(|e| fail(&format!("cannot read {input}: {e}")));
    let trace = picture::read(&text).unwrap_or_else(|e| fail(&format!("{input}: {e}")));
    print!("{}", picture::stats(&trace).text());
    let write = |path: &Option<String>, what: &str, body: String| {
        if let Some(path) = path {
            std::fs::write(path, body)
                .unwrap_or_else(|e| fail(&format!("cannot write {path}: {e}")));
            println!("{what}: {path}");
        }
    };
    write(&html, "html", picture::html(&trace));
    write(&perfetto, "perfetto", picture::chrome_trace(&trace));
    write(&svg, "svg", picture::svg(&trace, true));
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ! {
    eprintln!("cove-edge-timeline: {message}\n{USAGE}");
    std::process::exit(2)
}
