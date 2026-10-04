//! `cove-edge-load`: holds N requests in flight against a running server and
//! reports what came back.
//!
//! ```text
//! cargo run --release -p cove-edge --bin cove-edge-load -- \
//!     [--addr 127.0.0.1:8787] [--path /aggregate/] \
//!     [--concurrency 1000] [--requests 10000] [--threads 8] [--keep-alive]
//! ```
//!
//! std only, and not a thread per request on this side either: each of
//! `--threads` threads owns its share of the connections, writes each request
//! and then polls its sockets without blocking. Without `--keep-alive` each
//! request has a connection of its own, opened when it starts and closed by
//! the server when it is answered; with it, each of the `--concurrency`
//! connections asks its next request as soon as the last one is answered,
//! and is opened again only if the server closes it. The server's own counters — peak parked runs, peak
//! in-flight requests, its RSS — are read from `/_stats` after the run, having
//! been reset before it.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_edge::http::{closes, get, parse_response, request_bytes, response_length};
use cove_edge::os;

const USAGE: &str = "\
usage: cove-edge-load [--addr 127.0.0.1:8787] [--path /aggregate/]
                      [--concurrency 1000] [--requests 10000] [--threads 8]
                      [--keep-alive (reuse each connection)]";

/// How long one request may take before it is counted as failed.
const GIVE_UP: Duration = Duration::from_secs(30);

struct Outcome {
    latencies: Vec<Duration>,
    statuses: Vec<u16>,
    errors: Vec<String>,
    /// The first response that was not a 200, status and body.
    refused: Option<(u16, String)>,
    /// Connections opened.
    connections: usize,
}

struct Open {
    stream: TcpStream,
    raw: Vec<u8>,
    /// When the request being waited for was written.
    started: Instant,
}

fn main() {
    let mut addr = "127.0.0.1:8787".to_string();
    let mut path = "/aggregate/".to_string();
    let mut concurrency = 1000usize;
    let mut requests = 10_000usize;
    let mut threads = 8usize;
    let mut keep_alive = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| fail("a flag takes a value"));
        match arg.as_str() {
            "--addr" => addr = value(),
            "--path" => path = value(),
            "--concurrency" => concurrency = number(&value()),
            "--requests" => requests = number(&value()),
            "--threads" => threads = number(&value()),
            "--keep-alive" => keep_alive = true,
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
        "{requests} requests to http://{addr}{path}, {concurrency} in flight, {threads} client thread(s), {}",
        if keep_alive {
            "connections kept alive"
        } else {
            "a connection per request"
        }
    );

    let remaining = Arc::new(AtomicUsize::new(requests));
    let outcome = Arc::new(Mutex::new(Outcome {
        latencies: Vec::with_capacity(requests),
        statuses: Vec::with_capacity(requests),
        errors: Vec::new(),
        refused: None,
        connections: 0,
    }));
    let wall = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|at| {
            let share = concurrency / threads + usize::from(at < concurrency % threads);
            let (remaining, outcome) = (Arc::clone(&remaining), Arc::clone(&outcome));
            let request = request_bytes("GET", &path, "", keep_alive);
            std::thread::spawn(move || {
                drive(target, &request, keep_alive, share, &remaining, &outcome)
            })
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
            connections: 0,
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
    println!("  connections opened: {}", outcome.connections);
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
    keep_alive: bool,
    share: usize,
    remaining: &AtomicUsize,
    outcome: &Mutex<Outcome>,
) {
    let mut open: Vec<Open> = Vec::with_capacity(share);
    let mut latencies = Vec::new();
    let mut statuses = Vec::new();
    let mut errors = Vec::new();
    let mut refused = None;
    let mut connections = 0;
    let mut chunk = [0u8; 4096];
    loop {
        while open.len() < share && claim(remaining) {
            let started = Instant::now();
            connections += 1;
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
            // What reading this connection came to: nothing yet, a whole
            // response and whether the connection is still open, or an
            // error.
            let done = loop {
                if keep_alive {
                    if let Some(length) = response_length(&open[at].raw) {
                        let raw: Vec<u8> = open[at].raw.drain(..length).collect();
                        break Some(Ok((raw, true)));
                    }
                }
                match open[at].stream.read(&mut chunk) {
                    Ok(0) => break Some(Ok((std::mem::take(&mut open[at].raw), false))),
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
                // the run. Without keep-alive on macOS it is a real hazard:
                // a connect that lands on a tuple still in the server's
                // TIME_WAIT from the last run can stall.
                None if open[at].started.elapsed() > GIVE_UP => {
                    Some(Err(format!("no answer within {} s", GIVE_UP.as_secs())))
                }
                done => done,
            };
            let Some(result) = done else {
                at += 1;
                continue;
            };
            progressed = true;
            let mut reusable = false;
            match result.and_then(|(raw, still_open)| {
                reusable = still_open && !closes(&raw);
                parse_response(&raw)
            }) {
                Ok((status, body)) => {
                    latencies.push(open[at].started.elapsed());
                    statuses.push(status);
                    if status != 200 && refused.is_none() {
                        refused = Some((status, body));
                    }
                }
                Err(e) => {
                    reusable = false;
                    errors.push(e);
                }
            }
            // The next request on the same connection, if it may carry one
            // and there is one to make; otherwise the connection is done,
            // and the loop above opens a fresh one for the next request.
            if reusable && claim(remaining) {
                open[at].started = Instant::now();
                if let Err(e) = write_all_nonblocking(&mut open[at].stream, request.as_bytes()) {
                    errors.push(format!("write: {e}"));
                    open.swap_remove(at);
                }
                continue;
            }
            let finished = open.swap_remove(at);
            if reusable {
                reset(&finished.stream);
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
    outcome.connections += connections;
    if outcome.refused.is_none() {
        outcome.refused = refused;
    }
}

/// `write_all` on a socket in nonblocking mode: a request is a few dozen
/// bytes, so the send buffer is all but never full, but it is not an error
/// when it is.
fn write_all_nonblocking(stream: &mut TcpStream, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_micros(50))
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Makes dropping a kept-alive connection the generator has no more use for
/// a reset rather than a close, so that it leaves no `TIME_WAIT` behind.
///
/// The side that closes first holds the connection's `TIME_WAIT`, and
/// without keep-alive that is the server, which answers and closes. With it
/// the client closes, and its `TIME_WAIT`s hold their ephemeral ports for
/// twice the MSL — 30 s on macOS, whose range is 16,384 ports — so a second
/// 10,000-connection run straight after the first failed 13,718 of its
/// 20,000 connects with `EADDRNOTAVAIL`. A load generator abandoning
/// connections it opened is what a reset is for.
#[cfg(unix)]
fn reset(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // Safety: `setsockopt` reads `linger`, which outlives the call, for the
    // length it is told, on a descriptor `stream` owns.
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&linger as *const libc::linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
}

#[cfg(not(unix))]
fn reset(_stream: &TcpStream) {}

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
