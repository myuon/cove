//! Replays fuzz inputs without cargo-fuzz, and prints what the generator made.
//!
//! ```text
//! cove-fuzz <TARGET> FILE...                               replay each file through a target
//! cove-fuzz generate FILE...                               print the program FILE generates
//! cove-fuzz generate-differential FILE...                  the same, as the differential target generates it
//! cove-fuzz diff FILE...                                   a source's run in every mode, and whether they agree
//! cove-fuzz diff-generated FILE...                         the program a differential input generates, and the same
//! cove-fuzz check FILE...                                  print a source's diagnostics
//! cove-fuzz explain FILE...                                how a source ends on each tier
//! cove-fuzz explain-generated FILE...                      the same, for the program FILE generates
//! cove-fuzz image-seed OUT_DIR FILE...                     write image bodies of sources
//! ```

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        eprintln!(
            "usage: cove-fuzz <TARGET|generate|generate-differential|diff|diff-generated|check|explain|explain-generated|image-seed> FILE...\ntargets: {}",
            cove_fuzz::TARGETS.join(", ")
        );
        return ExitCode::from(2);
    };
    let read = |path: &str| std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    match command.as_str() {
        "generate" => {
            for path in rest {
                print!("{}", cove_fuzz::generate::from_bytes(&read(path)));
            }
        }
        "generate-differential" => {
            for path in rest {
                print!(
                    "{}",
                    cove_fuzz::generate::differential_from_bytes(&read(path))
                );
            }
        }
        // A stable report: the program, then each mode's observation, then
        // `agree` or the first mismatch. Exits non-zero on a mismatch, so a
        // script can minimise against it as well as libFuzzer can.
        "diff" | "diff-generated" => {
            let mut disagreed = false;
            for path in rest {
                let text = if command == "diff" {
                    String::from_utf8_lossy(&read(path)).into_owned()
                } else {
                    cove_fuzz::generate::differential_from_bytes(&read(path))
                };
                println!("=== {path}");
                if command == "diff-generated" {
                    println!("--- program ---\n{text}--- runs ---");
                }
                match cove_fuzz::differential::compare(&text) {
                    Ok(report) => {
                        print!("{}", report.describe());
                        disagreed |= report.mismatch.is_some();
                    }
                    Err(why) => println!("refused:\n{why}"),
                }
            }
            if disagreed {
                return ExitCode::FAILURE;
            }
        }
        "explain" => {
            for path in rest {
                let text = String::from_utf8_lossy(&read(path)).into_owned();
                for line in cove_fuzz::explain(&text) {
                    println!("{path}: {line}");
                }
            }
        }
        "explain-generated" => {
            for path in rest {
                let text = cove_fuzz::generate::from_bytes(&read(path));
                for line in cove_fuzz::explain(&text) {
                    println!("{path}: {line}");
                }
            }
        }
        "check" => {
            let mut failed = false;
            for path in rest {
                let text = String::from_utf8_lossy(&read(path)).into_owned();
                if let Err(why) = cove_fuzz::diagnose(&text) {
                    println!("{path}:\n{why}");
                    failed = true;
                }
            }
            if failed {
                return ExitCode::FAILURE;
            }
        }
        "image-seed" => {
            let Some((out, sources)) = rest.split_first() else {
                eprintln!("usage: cove-fuzz image-seed OUT_DIR FILE...");
                return ExitCode::from(2);
            };
            std::fs::create_dir_all(out).expect("create the output directory");
            let mut written = 0;
            for path in sources {
                let Ok(text) = String::from_utf8(read(path)) else {
                    continue;
                };
                if let Some(body) = cove_fuzz::image_body(&text) {
                    let name = path.replace(['/', '.'], "_");
                    std::fs::write(Path::new(out).join(name), body).expect("write a seed");
                    written += 1;
                }
            }
            eprintln!("wrote {written} image bodies to {out}");
        }
        target => {
            let Some(harness) = cove_fuzz::harness(target) else {
                eprintln!("no target `{target}`");
                return ExitCode::from(2);
            };
            for path in rest {
                harness(&read(path));
                eprintln!("{path}: ok");
            }
        }
    }
    ExitCode::SUCCESS
}
