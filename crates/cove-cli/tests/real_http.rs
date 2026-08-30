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
//! The other thing a fake cannot show is that a bound reaches a program that
//! is *waiting*, and two tests here are about that. A run's deadline has to
//! reach a program sitting inside `http.Server.handle` with a real socket and
//! no client: a fake listener returns of its own accord when its queue is
//! empty, so a program serving against one would stop whether or not the
//! run's controls reached the host at all. And a `clock.timeout` has to reach
//! a program sitting inside `http.fetch` with a real socket and a server that
//! has gone quiet — issue #170, where the flag was raised on time and nothing
//! in the client was reading it. Both assert on how long the run took, since
//! the wrong implementation reaches the right answer eventually.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::SourceMap;
use cove_runtime::budget::{Budget, Limits};
use cove_runtime::clock::Clock;
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

    let mut hosts = HostRegistry::new(Grants::new(["http", "console", "clock"]));
    hosts.register(Box::new(Console::new(std::io::sink(), std::io::sink())));
    hosts.register(Box::new(Http::real()));
    // A real clock, because `clock.timeout` on a virtual one judges afterwards
    // by how far the body moved time and a `fetch` moves it not at all. The
    // bound these tests are about is the one that raises a flag on a thread
    // while the host is waiting, and only the real clock has that.
    hosts.register(Box::new(Clock::real()));
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

/// The program `examples/covecheck` writes around each of its fetches, with
/// the URL it is given: one `clock.timeout` around one `http.fetch`, and the
/// three outcomes told apart rather than collapsed into a message.
const BOUNDED_FETCH: &str = r#"use clock
use http

/// Fetches one URL under a bound, and answers which of the three happened.
export fn main() -> Result<String, Error> {
  let answer: Result<Result<http.Response, Error>, Error> = clock.timeout(300ms) {
    http.fetch("THE_URL")
  }

  match answer {
    Ok(fetched) => match fetched {
      Ok(response) => Ok("answered {response.status}")
      Err(error) => Ok("unanswered {error.message}")
    }
    Err(error) => Ok("bounded {error.message}")
  }
}
"#;

/// Binds loopback, reads one request, and then says nothing until it is told
/// to hang up.
///
/// A server that answers nothing is the only one that can show this bug: the
/// client has to be *inside* its read, with the connection open and the
/// request sent, when the bound is raised. The connection is held rather than
/// dropped, because dropping it is an end-of-file and would end the client's
/// read for a reason that has nothing to do with the bound.
fn stall_once(hung_up: Arc<AtomicBool>) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is available");
    let port = listener
        .local_addr()
        .expect("a bound socket has an address")
        .port();
    let thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the program connects");
        read_request(&stream);
        while !hung_up.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(stream);
    });
    (port, thread)
}

/// A `clock.timeout` around a `fetch` cuts the fetch short.
///
/// This is issue #170 end to end, and nothing here stands in for anything:
/// the bound is written in Cove, the flag is raised by the real clock's own
/// watchdog thread on a thread of its own, and the client is a real socket
/// waiting on a real server that has decided to say nothing. Before this
/// change the client folded the run's deadline into one socket timeout and
/// then blocked, so the flag had nobody reading it and the program was told
/// about its 300ms bound thirty seconds later, when `READ_TIMEOUT` expired.
///
/// The assertion is therefore about *when* and not only about *what*: the old
/// client produced this same answer, eventually. Five seconds is the line,
/// which is more than sixteen times the bound — room for a loaded machine to
/// be slow in — and a sixth of the thirty seconds the unfixed client takes.
#[test]
fn a_clock_timeout_cuts_short_a_fetch_waiting_on_a_silent_server() {
    let hung_up = Arc::new(AtomicBool::new(false));
    let (port, server) = stall_once(Arc::clone(&hung_up));
    let bound = Duration::from_millis(300);

    let source = BOUNDED_FETCH.replace("THE_URL", &format!("http://127.0.0.1:{port}/health"));
    let (ran, took) = run_under("fetch-timeout", &source, Limits::default());

    hung_up.store(true, Ordering::Relaxed);
    server.join().expect("the server thread finishes");

    let value = ran.expect("the program ran without a runtime error");
    assert_eq!(
        outcome(&value),
        Ok("bounded clock: timed out after 300ms".to_string()),
        "the bound the program wrote is what it is told about"
    );
    assert!(
        took >= bound,
        "the fetch ended before the bound that was supposed to end it, after {took:?}"
    );
    assert!(
        took < Duration::from_secs(5),
        "the fetch waited on `READ_TIMEOUT` rather than on its `clock.timeout`, for {took:?}"
    );
}
