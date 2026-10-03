//! `cove-edge`: the server.
//!
//! ```text
//! cargo run --release -p cove-edge -- [--port 8787] [--workers 4]
//!     [--latency 20..100] [--pool N] [--quiet] [--tenants DIR]
//! ```

use std::time::Duration;

use cove_edge::{os, DeployOptions, Isolates, Latency, Server, ServerOptions, State};

const USAGE: &str = "\
usage: cove-edge [--port 8787] [--host 127.0.0.1] [--workers 4]
                 [--latency MIN..MAX (ms, default 20..100)]
                 [--pool N (resident isolates per tenant; default: fresh per request)]
                 [--blocking-upstream (sleep on the worker instead of parking)]
                 [--quiet (no `log.info` lines)] [--tenants DIR]";

fn main() {
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

    let mut args = std::env::args().skip(1);
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
            "--latency" => {
                let text = value("--latency");
                let (min, max) = text.split_once("..").unwrap_or((&text, &text));
                latency = Latency {
                    min: Duration::from_millis(parse(min)),
                    max: Duration::from_millis(parse(max)),
                };
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
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
         upstream latency {}..{} ms ({}), open-file limit {files}",
        latency.min.as_millis(),
        latency.max.as_millis(),
        if blocking_upstream {
            "blocking the worker"
        } else {
            "parked"
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
