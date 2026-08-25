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
//! the client sent nothing at all.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::host::{Console, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::interp::Interpreter;
use cove_runtime::runtime::Runtime;
use cove_runtime::value::Value;

/// A temporary package directory that removes itself when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cove-real-http-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
fn fetch(url: &str) -> Value {
    let dir = TempDir::new("fetch");
    let source = format!(
        "use http\n\n\
         /// Fetches one URL and answers what came back.\n\
         export fn main() -> Result<String, Error> {{\n  \
         let body = http.fetch(\"{url}\")?\n  \
         Ok(body)\n\
         }}\n"
    );
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::write(dir.path().join("app/main.cove"), source).unwrap();
    std::fs::write(dir.path().join("cove.toml"), "").unwrap();

    let mut sources = SourceMap::new();
    let package = cove_sema::package::load(dir.path(), &mut sources).expect("the package loads");
    let program = cove_sema::resolve::resolve(&package).expect("the package resolves");

    let mut hosts = HostRegistry::new(Grants::new(["http", "console"]));
    hosts.register(Box::new(Console::new(std::io::sink())));
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

/// A Cove program reaching a real server, over a real socket.
#[test]
fn a_program_fetches_a_body_from_a_server_on_loopback() {
    let (port, server) = serve_once("200 OK", "{\"status\":\"ok\"}");

    let value = fetch(&format!("http://127.0.0.1:{port}/health"));
    assert_eq!(outcome(&value), Ok("{\"status\":\"ok\"}".to_string()));

    let asked = server.join().expect("the server thread finishes");
    assert_eq!(asked.method, "GET");
    assert_eq!(asked.target, "/health");
}

/// A status the server refused with is an ordinary Cove failure, not a body.
#[test]
fn a_program_is_told_when_a_server_answers_a_failure_status() {
    let (port, server) = serve_once("503 Service Unavailable", "busy");

    let value = fetch(&format!("http://127.0.0.1:{port}/health"));
    let Err(message) = outcome(&value) else {
        panic!("a 503 is a failure, not a body: {value}");
    };
    assert!(message.ends_with("answered 503"), "{message}");

    server.join().expect("the server thread finishes");
}
