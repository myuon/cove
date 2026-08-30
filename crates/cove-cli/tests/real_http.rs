//! A Cove program against the real `http` host, over loopback.
//!
//! Every other test of a host that reaches the outside world reaches a fake
//! instead, because a suite that depended on the network would be a suite that
//! failed for reasons that were not the program's. This one does not: the test
//! is the server. It binds `127.0.0.1:0`, lets the operating system choose the
//! port, and answers exactly one request from a thread of its own, so the real
//! client — sockets, an HTTP/1.1 request line, headers, a status line, a body
//! — runs against something that is genuinely on the other end of a
//! connection and is still entirely inside this process.
//!
//! What it proves that a fake cannot is that `http.fetch` speaks the protocol.
//! A recorded fake answers a URL from a table and would keep answering it if
//! the client sent nothing at all. That now includes the status: `fetch`
//! answers an `http.Response`, and the number in it has to have been read off
//! a real status line for these tests to pass, which is the half of issue
//! #145 a table of canned answers cannot vouch for.
//!
//! The last test here is the other thing a fake cannot show: that a run's
//! deadline reaches a program sitting inside `http.Server.handle` with a real
//! socket and no client. A fake listener returns of its own accord when its
//! queue is empty, so a program serving against one would stop whether or not
//! the run's controls reached the host at all.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::SourceMap;
use cove_runtime::budget::{Budget, Limits};
use cove_runtime::host::{Console, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::interp::Interpreter;
use cove_runtime::runtime::Runtime;
use cove_runtime::value::Value;

/// A temporary package directory that removes itself when the test ends.
struct TempDir(PathBuf);

/// Distinguishes two directories asked for in the same nanosecond.
///
/// The tests below run on threads of one process, and the clock is coarse
/// enough that two of them can read the same instant. Two `TempDir`s that
/// agreed on a name would share one package -- so both programs would fetch
/// whichever URL was written last, and the first to finish would delete the
/// directory out from under the other.
static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cove-real-http-{name}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
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

/// One request this test served, so the assertions can be about the wire and
/// not only about the answer.
struct Asked {
    method: String,
    target: String,
}

/// Binds loopback, answers one request with `status` and `body`, and reports
/// what was asked for.
fn serve_once(status: &str, body: &'static str) -> (u16, std::thread::JoinHandle<Asked>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is available");
    let port = listener
        .local_addr()
        .expect("a bound socket has an address")
        .port();
    let status = status.to_string();
    let thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the program connects");
        let asked = read_request(&stream);
        let answer = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut out = &stream;
        out.write_all(answer.as_bytes())
            .expect("the answer is sent");
        out.flush().expect("the answer is flushed");
        asked
    });
    (port, thread)
}

/// Reads a request line and drains the headers, which is all this test needs
/// of the client's side of the protocol.
fn read_request(stream: &TcpStream) -> Asked {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("a request line arrives");
    let mut parts = line.split_whitespace();
    let asked = Asked {
        method: parts.next().unwrap_or_default().to_string(),
        target: parts.next().unwrap_or_default().to_string(),
    };
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    asked
}

/// Runs a one-module package that fetches `url`, against the real host.
///
/// The program answers `<status> <body>` rather than the response itself,
/// because a `Value` a test reads back is easiest to assert on as text and
/// because writing the two together is what says the program saw both. What
/// it proves is that the status crossed the whole boundary: off a real socket,
/// through `http.fetch`'s declared `Result<http.Response, Error>`, past the
/// schema check the registry makes on the way out, and into an interpolation
/// in Cove source.
fn fetch(url: &str) -> Value {
    let dir = TempDir::new("fetch");
    let source = format!(
        "use http\n\n\
         /// Fetches one URL and answers what came back.\n\
         export fn main() -> Result<String, Error> {{\n  \
         let answer = http.fetch(\"{url}\")?\n  \
         Ok(\"{{answer.status}} {{answer.body}}\")\n\
         }}\n"
    );
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::write(dir.path().join("app/main.cove"), source).unwrap();
    std::fs::write(dir.path().join("cove.toml"), "").unwrap();

    let mut sources = SourceMap::new();
    let package = cove_sema::package::load(dir.path(), &mut sources).expect("the package loads");
    let program = cove_sema::resolve::resolve(&package).expect("the package resolves");

    let mut hosts = HostRegistry::new(Grants::new(["http", "console"]));
    hosts.register(Box::new(Console::new(std::io::sink(), std::io::sink())));
    hosts.register(Box::new(Http::real()));

    let runtime = Runtime::new(Arc::new(program), Arc::new(sources), Arc::new(hosts));
    Interpreter::new(&runtime)
        .run_entry("app", "main", Vec::<Rc<str>>::new())
        .expect("the program ran without a runtime error")
}

/// The `Ok` payload, or the message an `Err` carried.
fn outcome(value: &Value) -> Result<String, String> {
    let Value::Enum(result) = value else {
        panic!("expected a `Result`, found {value}");
    };
    let payload = result
        .payload
        .first()
        .map(ToString::to_string)
        .unwrap_or_default();
    match &*result.case {
        "Ok" => Ok(payload),
        _ => Err(payload),
    }
}

/// The program `examples/server/` is, on a port the operating system picks
/// and with nobody ever connecting to it.
const SERVER: &str = r#"use http

/// Answers the one route this server has.
export fn health(request: http.Request) -> http.Response {
  http.json(200, "ok")
}

/// The routing table, built once before anything is served.
fn routes() -> Array<http.Route> {
  var building = Vector.of()

  building.push(http.Route(
    method: http.Method.Get,
    path: "/health",
    handler: health,
  ))

  building.freeze()
}

/// Serves until the listener has nothing more to serve.
export fn main() -> Result<Unit, Error> {
  let server = http.listen(0)?
  let table = routes()

  while server.handle(table)? {
  }

  server.close()?
  Ok(())
}
"#;

/// Runs a one-module package against the real host under `limits`, and
/// answers what the run came to and how long it took.
fn run_under(name: &str, source: &str, limits: Limits) -> (Result<Value, String>, Duration) {
    let dir = TempDir::new(name);
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::write(dir.path().join("app/main.cove"), source).unwrap();
    std::fs::write(dir.path().join("cove.toml"), "").unwrap();

    let mut sources = SourceMap::new();
    let package = cove_sema::package::load(dir.path(), &mut sources).expect("the package loads");
    let program = cove_sema::resolve::resolve(&package).expect("the package resolves");

    let mut hosts = HostRegistry::new(Grants::new(["http", "console"]));
    hosts.register(Box::new(Console::new(std::io::sink(), std::io::sink())));
    hosts.register(Box::new(Http::real()));
    hosts.set_budget(Budget::new(limits));

    let runtime = Runtime::new(Arc::new(program), Arc::new(sources), Arc::new(hosts));
    let started = Instant::now();
    let outcome = Interpreter::new(&runtime)
        .run_entry("app", "main", Vec::<Rc<str>>::new())
        .map_err(|error| error.message);
    (outcome, started.elapsed())
}

/// A run's deadline reaches a program waiting inside `http.Server.handle`.
///
/// This is the end-to-end half of what `cove-runtime`'s own tests check with
/// a stub: nothing here stands in for the interpreter, so the deadline
/// travels the whole way — from `Limits` into the run's `Budget`, out through
/// the `Reentry` the host is handed, and into the loop that polls the
/// listener. Without that path the program never returns and this test hangs
/// rather than failing, which is the honest shape for it: a server waiting
/// for a client that will never come is exactly the bug.
#[test]
fn a_run_deadline_stops_a_program_waiting_for_a_connection() {
    let deadline = Duration::from_millis(300);
    let (outcome, took) = run_under(
        "serve-deadline",
        SERVER,
        Limits {
            deadline: Some(deadline),
            ..Limits::default()
        },
    );

    let Err(message) = outcome else {
        panic!("a server with no client cannot finish on its own: {outcome:?}");
    };
    assert!(
        message.contains("wall-clock deadline of 300ms exceeded"),
        "the run stops for the reason the budget holds, not one the host invented: {message}"
    );
    assert!(
        took >= deadline,
        "the run stopped before its deadline, after {took:?}"
    );
    assert!(
        took < Duration::from_secs(5),
        "the run outlived its deadline by {took:?}"
    );
}

/// A Cove program reaching a real server, over a real socket.
#[test]
fn a_program_fetches_a_response_from_a_server_on_loopback() {
    let (port, server) = serve_once("200 OK", "{\"status\":\"ok\"}");

    let value = fetch(&format!("http://127.0.0.1:{port}/health"));
    assert_eq!(outcome(&value), Ok("200 {\"status\":\"ok\"}".to_string()));

    let asked = server.join().expect("the server thread finishes");
    assert_eq!(asked.method, "GET");
    assert_eq!(asked.target, "/health");
}

/// A status the server refused with reaches the program as a status.
///
/// This is the same program and the same socket as the test above, and the
/// only thing that differs is the number the server put on its status line,
/// which is the point of issue #145. It used to be an ordinary Cove failure
/// carrying the message `http: {url} answered 503`, so a program had prose
/// where the status was and could not tell this run from one where nothing
/// answered at all. It is now an answer, and the body the `503` carried comes
/// with it -- which a failure could not have carried either, and which is
/// usually where a server says what went wrong.
#[test]
fn a_program_is_told_the_failure_status_a_server_answered_with() {
    let (port, server) = serve_once("503 Service Unavailable", "busy");

    let value = fetch(&format!("http://127.0.0.1:{port}/health"));
    assert_eq!(outcome(&value), Ok("503 busy".to_string()));

    server.join().expect("the server thread finishes");
}

/// A connection nothing is listening on is still a failure, and that is what
/// keeps the two apart.
///
/// The pair this makes with the test above is the whole of what issue #145
/// asked for: one URL answered `503` and one was never reached, and a program
/// tells them apart by the shape of what it was handed rather than by reading
/// the message. The port is bound and then dropped, so it is one nothing is
/// listening on rather than one that might belong to something else.
#[test]
fn a_program_is_told_when_no_server_answered_at_all() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is available");
        listener
            .local_addr()
            .expect("a bound socket has an address")
            .port()
    };

    let value = fetch(&format!("http://127.0.0.1:{port}/health"));
    let Err(message) = outcome(&value) else {
        panic!("nothing was listening, so there is no response: {value}");
    };
    assert!(
        message.starts_with(&format!("http: cannot connect to 127.0.0.1:{port}")),
        "{message}"
    );
}
