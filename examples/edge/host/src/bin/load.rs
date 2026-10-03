//! `cove-edge-load`: holds N requests in flight against a running server and
//! reports what came back.
//!
//! ```text
//! cargo run --release -p cove-edge --bin cove-edge-load -- \
//!     [--addr 127.0.0.1:8787] [--path /aggregate/] \
//!     [--concurrency 1000] [--requests 10000] [--threads 8]
//! ```
//!
//! std only, and not a thread per request on this side either: each of
//! `--threads` threads owns its share of the connections, writes each request
//! and then polls its sockets without blocking, opening a new connection as
//! each one answers. The server's own counters — peak parked runs, peak
//! in-flight requests, its RSS — are read from `/_stats` after the run, having
//! been reset before it.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_edge::http::{get, parse_response, request_bytes};
use cove_edge::os;

const USAGE: &str = "\
usage: cove-edge-load [--addr 127.0.0.1:8787] [--path /aggregate/]
                      [--concurrency 1000] [--requests 10000] [--threads 8]";

/// How long one request may take before it is counted as failed.
const GIVE_UP: Duration = Duration::from_secs(30);

struct Outcome {
    latencies: Vec<Duration>,
    statuses: Vec<u16>,
    errors: Vec<String>,
    /// The first response that was not a 200, status and body.
    refused: Option<(u16, String)>,
}

struct Open {
    stream: TcpStream,
    raw: Vec<u8>,
    started: Instant,
}

fn main() {
    let mut addr = "127.0.0.1:8787".to_string();
    let mut path = "/aggregate/".to_string();
    let mut concurrency = 1000usize;
    let mut requests = 10_000usize;
    let mut threads = 8usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| fail("a flag takes a value"));
        match arg.as_str() {
            "--addr" => addr = value(),
            "--path" => path = value(),
            "--concurrency" => concurrency = number(&value()),
            "--requests" => requests = number(&value()),
            "--threads" => threads = number(&value()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown argument `{other}`")),
        }
    }
    let target: SocketAddr = addr
        .to_socket_addrs()
        .ok()
        .and_then(|mut found| found.next())
        .unwrap_or_else(|| fail(&format!("cannot resolve `{addr}`")));
    let files = os::raise_open_files();
    let concurrency = concurrency.min(requests).max(1);
    let threads = threads.min(concurrency).max(1);
    if files < concurrency as u64 + 64 {
        eprintln!("warning: the open-file limit is {files}, below {concurrency} connections");
    }

    get(target, "/_stats?reset").unwrap_or_else(|e| fail(&format!("is the server up? {e}")));
    println!(
        "{requests} requests to http://{addr}{path}, {concurrency} in flight, {threads} client thread(s)"
    );

    let remaining = Arc::new(AtomicUsize::new(requests));
    let outcome = Arc::new(Mutex::new(Outcome {
        latencies: Vec::with_capacity(requests),
        statuses: Vec::with_capacity(requests),
        errors: Vec::new(),
        refused: None,
    }));
    let wall = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|at| {
            let share = concurrency / threads + usize::from(at < concurrency % threads);
            let (remaining, outcome) = (Arc::clone(&remaining), Arc::clone(&outcome));
            let request = request_bytes("GET", &path, "");
            std::thread::spawn(move || drive(target, &request, share, &remaining, &outcome))
        })
        .collect();
    for handle in handles {
        handle.join().expect("a client thread");
    }
    let wall = wall.elapsed();

    let mut outcome = std::mem::replace(
        &mut *outcome.lock().unwrap(),
        Outcome {
            latencies: Vec::new(),
            statuses: Vec::new(),
            errors: Vec::new(),
            refused: None,
        },
    );
    outcome.latencies.sort();
    let ok = outcome.statuses.iter().filter(|&&s| s == 200).count();
    let pct = |p: f64| {
        let all = &outcome.latencies;
        if all.is_empty() {
            return 0.0;
        }
        all[((all.len() - 1) as f64 * p).round() as usize].as_secs_f64() * 1e3
    };
    println!(
        "  answered {} ({ok} with 200), {} failed to connect or read, in {:.2} s: {:.0} req/s",
        outcome.statuses.len(),
        outcome.errors.len(),
        wall.as_secs_f64(),
        outcome.statuses.len() as f64 / wall.as_secs_f64()
    );
    println!(
        "  latency ms: p50 {:.1}  p90 {:.1}  p99 {:.1}  max {:.1}",
        pct(0.5),
        pct(0.9),
        pct(0.99),
        pct(1.0)
    );
    if let Some(first) = outcome.errors.first() {
        println!("  first error: {first}");
    }
    if let Some((status, body)) = &outcome.refused {
        println!("  first non-200 ({status}): {}", body.trim_end());
    }
    match get(target, "/_stats") {
        Ok((_, body)) => println!("server /_stats after the run:\n{body}"),
        Err(e) => println!("server /_stats unreadable: {e}"),
    }
}

/// Keeps `share` requests in flight until `remaining` runs out.
fn drive(
    target: SocketAddr,
    request: &str,
    share: usize,
    remaining: &AtomicUsize,
    outcome: &Mutex<Outcome>,
) {
    let mut open: Vec<Open> = Vec::with_capacity(share);
    let mut latencies = Vec::new();
    let mut statuses = Vec::new();
    let mut errors = Vec::new();
    let mut refused = None;
    let mut chunk = [0u8; 4096];
    loop {
        while open.len() < share && claim(remaining) {
            let started = Instant::now();
            let opened = TcpStream::connect_timeout(&target, Duration::from_secs(10)).and_then(
                |mut stream| {
                    stream.write_all(request.as_bytes())?;
                    stream.set_nonblocking(true)?;
                    Ok(stream)
                },
            );
            match opened {
                Ok(stream) => open.push(Open {
                    stream,
                    raw: Vec::with_capacity(512),
                    started,
                }),
                Err(e) => errors.push(format!("connect: {e}")),
            }
        }
        if open.is_empty() {
            break;
        }
        let mut progressed = false;
        let mut at = 0;
        while at < open.len() {
            let done = loop {
                match open[at].stream.read(&mut chunk) {
                    Ok(0) => break Some(Ok(())),
                    Ok(n) => {
                        open[at].raw.extend_from_slice(&chunk[..n]);
                        progressed = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break None,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => break Some(Err(format!("read: {e}"))),
                }
            };
            let done = match done {
                // A connection that has not answered in this long is given
                // up on and counted, so that a lost connection cannot hang
                // the run. On macOS it is a real hazard: a connect that lands
                // on a tuple still in the server's TIME_WAIT from the last
                // run can stall.
                None if open[at].started.elapsed() > GIVE_UP => {
                    Some(Err(format!("no answer within {} s", GIVE_UP.as_secs())))
                }
                done => done,
            };
            match done {
                None => at += 1,
                Some(result) => {
                    progressed = true;
                    let finished = open.swap_remove(at);
                    match result.and_then(|()| parse_response(&finished.raw)) {
                        Ok((status, body)) => {
                            latencies.push(finished.started.elapsed());
                            statuses.push(status);
                            if status != 200 && refused.is_none() {
                                refused = Some((status, body));
                            }
                        }
                        Err(e) => errors.push(e),
                    }
                }
            }
        }
        if !progressed {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    let mut outcome = outcome.lock().unwrap();
    outcome.latencies.extend(latencies);
    outcome.statuses.extend(statuses);
    outcome.errors.extend(errors);
    if outcome.refused.is_none() {
        outcome.refused = refused;
    }
}

fn claim(remaining: &AtomicUsize) -> bool {
    remaining
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_ok()
}

fn number(text: &str) -> usize {
    text.parse()
        .unwrap_or_else(|_| fail(&format!("`{text}` is not a number")))
}

fn fail(message: &str) -> ! {
    eprintln!("cove-edge-load: {message}\n{USAGE}");
    std::process::exit(2)
}
