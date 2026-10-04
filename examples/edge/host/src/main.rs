//! `cove-edge`: the server, and the checker and test runner for its tenants.
//!
//! ```text
//! cargo run --release -p cove-edge -- [--port 8787] [--workers 4]
//!     [--latency 20..100] [--pool N] [--quiet] [--tenants DIR]
//! cargo run --release -p cove-edge -- check [tenant…] [--tenants DIR]
//! cargo run --release -p cove-edge -- test [tenant…] [--filter TEXT]
//!     [--latency MIN..MAX] [--tenants DIR]
//! ```

use std::process::ExitCode;
use std::time::Duration;

use cove_edge::toolchain::{self, TestOptions};
use cove_edge::{os, DeployOptions, Isolates, KeepAlive, Latency, Server, ServerOptions, State};

const USAGE: &str = "\
usage: cove-edge [--port 8787] [--host 127.0.0.1] [--workers 4]
                 [--latency MIN..MAX (ms, default 20..100)]
                 [--pool N (resident isolates per tenant; default: fresh per request)]
                 [--blocking-upstream (sleep on the worker instead of parking)]
                 [--quiet (no `log.info` lines)] [--tenants DIR]
                 [--no-keep-alive] [--idle-timeout MS (default 5000)]
                 [--max-requests N (per connection, default 1000)]
       cove-edge check [tenant…] [--tenants DIR]
       cove-edge test [tenant…] [--filter TEXT] [--latency MIN..MAX (ms, default 0)]
                      [--tenants DIR]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check") => tool(&args[1..], false),
        Some("test") => tool(&args[1..], true),
        _ => serve(args),
    }
}

/// `check` and `test`: the toolchain, with the server's schemas and hosts.
fn tool(args: &[String], test: bool) -> ExitCode {
    let mut tenants = cove_edge::tenants_root();
    let mut only = Vec::new();
    let mut filter = None;
    let mut latency = Latency {
        min: Duration::ZERO,
        max: Duration::ZERO,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .unwrap_or_else(|| fail(&format!("`{name}` takes a value")))
        };
        match arg.as_str() {
            "--tenants" => tenants = value("--tenants").into(),
            "--filter" if test => filter = Some(value("--filter")),
            "--latency" if test => latency = parse_latency(&value("--latency")),
            flag if flag.starts_with('-') => fail(&format!("unknown argument `{flag}`")),
            name => only.push(name.to_string()),
        }
    }
    let report = if test {
        toolchain::test(&tenants, &only, &TestOptions { latency, filter })
    } else {
        toolchain::check(&tenants, &only)
    }
    .unwrap_or_else(|why| fail(&why));
    eprint!("{}", report.err);
    print!("{}", report.out);
    if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn parse_latency(text: &str) -> Latency {
    let (min, max) = text.split_once("..").unwrap_or((text, text));
    Latency {
        min: Duration::from_millis(parse(min)),
        max: Duration::from_millis(parse(max)),
    }
}

fn serve(args: Vec<String>) -> ExitCode {
    let mut port = 8787u16;
    let mut host = "127.0.0.1".to_string();
    let mut workers = 4usize;
    let mut latency = Latency {
        min: Duration::from_millis(20),
        max: Duration::from_millis(100),
    };
    let mut isolates = Isolates::PerRequest;
    let mut quiet = false;
    let mut blocking_upstream = false;
    let mut tenants = cove_edge::tenants_root();
    let mut keep_alive = KeepAlive::default();

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .unwrap_or_else(|| fail(&format!("`{name}` takes a value")))
        };
        match arg.as_str() {
            "--port" => port = parse(&value("--port")),
            "--host" => host = value("--host"),
            "--workers" => workers = parse(&value("--workers")),
            "--pool" => isolates = Isolates::Pooled(parse(&value("--pool"))),
            "--quiet" => quiet = true,
            "--blocking-upstream" => blocking_upstream = true,
            "--tenants" => tenants = value("--tenants").into(),
            "--latency" => latency = parse_latency(&value("--latency")),
            "--no-keep-alive" => keep_alive.enabled = false,
            "--idle-timeout" => {
                keep_alive.idle = Duration::from_millis(parse(&value("--idle-timeout")))
            }
            "--max-requests" => keep_alive.max_requests = parse(&value("--max-requests")),
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => fail(&format!("unknown argument `{other}`")),
        }
    }

    let files = os::raise_open_files();
    println!("cove-edge: deploying tenants from {}", tenants.display());
    let server = Server::start(ServerOptions {
        listen: format!("{host}:{port}"),
        workers,
        isolates,
        keep_alive,
        deploy: DeployOptions {
            tenants,
            latency,
            quiet,
            blocking_upstream,
        },
    })
    .unwrap_or_else(|why| fail(&why));

    for tenant in server.tenants() {
        println!("  {}", tenant.describe());
    }
    let addr = server.addr;
    println!(
        "\nlistening on http://{addr} — {workers} worker thread(s), {isolates}, \
         upstream latency {}..{} ms ({}), {}, open-file limit {files}",
        latency.min.as_millis(),
        latency.max.as_millis(),
        if blocking_upstream {
            "blocking the worker"
        } else {
            "parked"
        },
        if keep_alive.enabled {
            format!(
                "keep-alive (idle {} ms, {} requests per connection)",
                keep_alive.idle.as_millis(),
                keep_alive.max_requests
            )
        } else {
            "no keep-alive".to_string()
        },
    );
    println!("\ntry:");
    for tenant in server.tenants() {
        if let State::Deployed(_) = tenant.state {
            let example = match tenant.name.as_str() {
                "hello" => "hello/?name=Cove",
                "counter" => "counter/home",
                "aggregate" => "aggregate/",
                _ => "",
            };
            if !example.is_empty() {
                println!("  curl -s http://{addr}/{example}");
            }
        }
    }
    println!("  curl -s http://{addr}/_stats\n");

    loop {
        std::thread::park();
    }
}

fn parse<T: std::str::FromStr>(text: &str) -> T {
    text.trim()
        .parse()
        .unwrap_or_else(|_| fail(&format!("`{text}` is not a number")))
}

fn fail(message: &str) -> ! {
    eprintln!("cove-edge: {message}\n{USAGE}");
    std::process::exit(2)
}
