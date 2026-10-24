//! A worker and a resident isolate reused after a tenant failed: issue
//! [#681](https://github.com/myuon/cove/issues/681), priority 3, at the scale
//! of a whole server.
//!
//! `crates/cove-runtime/tests/lifecycle.rs` holds one machine to "a failed
//! run leaves nothing the next run can see". This holds the edge server to
//! it where the reuse is the server's own choice: **one** worker thread runs
//! every request, and `Isolates::Pooled(1)` hands each tenant's next request
//! the very `OwnedVm` its last one ran on — including one that trapped, ran
//! out of heap, or was stopped by its deadline while running or while parked
//! at a fetch whose answer then arrived late. Every answer after a failure
//! must be the answer a fresh server gives, and two tenants running the same
//! code must not see each other's state.
//!
//! The tenants are written to a directory of the test's own, beside copies
//! of three of the shipped ones. Nothing here asserts a duration.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cove_edge::http::get;
use cove_edge::{
    Backend, DeployOptions, Discipline, Isolates, KeepAlive, Latency, Server, ServerOptions, State,
};

const MANIFEST: &str = r#"
[run.hello]
entry = "hello.handle"
allow = []
deadline = "200ms"

[run.counterA]
entry = "counter.handle"
allow = ["kv", "log"]

[run.counterB]
entry = "counter.handle"
allow = ["kv", "log"]

[run.trap]
entry = "trap.handle"
allow = []

[run.hog]
entry = "hog.handle"
allow = []
deadline = "30s"

[run.proxy]
entry = "proxy.handle"
allow = ["upstream"]
deadline = "300ms"
max_host_calls = 4

[run.greedy]
entry = "greedy.handle"
allow = ["kv"]
"#;

const POLICY: &str = r#"
[proxy]
fetch = ["127.0.0.1"]
"#;

const TRAP: &str = r#"use edge

/// Answers `/ok`, and divides ten by `?n=` anywhere else.
export fn handle(request: edge.Request) -> edge.Response {
  let n = Int.parse(request.query.get("n").unwrapOr("0")).unwrapOr(0)
  if request.path == "/ok" {
    return edge.Response(status: 200, contentType: "text/plain", body: "fine {n}\n")
  }
  edge.Response(status: 200, contentType: "text/plain", body: "{10 / n}\n")
}
"#;

const HOG: &str = r#"use edge

/// Keeps `?n=` numbers alive at once.
export fn handle(request: edge.Request) -> edge.Response {
  let n = Int.parse(request.query.get("n").unwrapOr("0")).unwrapOr(0)
  var items = Vector.of(0)
  for i in 0..<n {
    items.push(i)
  }
  edge.Response(status: 200, contentType: "text/plain", body: "kept {items.length()}\n")
}
"#;

/// A tenants directory of this test's own.
fn tenants() -> PathBuf {
    let root = std::env::temp_dir().join(format!("cove-edge-isolation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("cove.toml"), MANIFEST).unwrap();
    std::fs::write(root.join("edge.toml"), POLICY).unwrap();
    let shipped = cove_edge::tenants_root();
    for name in ["hello", "counter", "proxy", "greedy"] {
        copy(
            &shipped.join(name).join(format!("{name}.cove")),
            &root.join(name),
        );
    }
    for (name, text) in [("trap", TRAP), ("hog", HOG)] {
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(root.join(name).join(format!("{name}.cove")), text).unwrap();
    }
    root
}

fn copy(file: &Path, into: &Path) {
    std::fs::create_dir_all(into).unwrap();
    std::fs::copy(file, into.join(file.file_name().unwrap())).unwrap();
}

fn start(root: &Path, isolates: Isolates) -> Server {
    Server::start(ServerOptions {
        listen: "127.0.0.1:0".to_string(),
        workers: 1,
        isolates,
        keep_alive: KeepAlive::default(),
        fetchers: 1,
        timeline: None,
        scheduler: Discipline::Stealing,
        slice: None,
        deploy: DeployOptions {
            tenants: root.to_path_buf(),
            latency: Latency {
                min: Duration::from_millis(1),
                max: Duration::from_millis(1),
            },
            quiet: true,
            blocking_upstream: false,
            backend: Backend::Vm,
        },
    })
    .expect("the server starts")
}

fn stat(server: &Server, key: &str) -> i64 {
    let (_, body) = get(server.addr, "/_stats").expect("stats answer");
    let needle = format!("\"{key}\": ");
    let at = body.find(&needle).expect("the key is reported") + needle.len();
    body[at..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|n| n.parse().ok())
        .expect("a number")
}

/// An upstream that reads one request and answers it after `after` — later
/// than `proxy`'s deadline — and says whether the write went through.
fn late_upstream(after: Duration) -> (u16, std::sync::mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&chunk[..n]),
            }
        }
        std::thread::sleep(after);
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nlate");
        let _ = done.send(());
    });
    (port, finished)
}

#[test]
fn a_worker_and_its_resident_isolates_serve_on_after_every_kind_of_failure() {
    let root = tenants();
    let fresh = start(&root, Isolates::PerRequest);
    let server = start(&root, Isolates::Pooled(1));

    // Each tenant's registry grants exactly its own table, and the one that
    // over-reaches is refused at deploy whatever was deployed beside it.
    for tenant in server.tenants() {
        match (&tenant.state, tenant.name.as_str()) {
            (State::Deployed(deployed), _) => {
                let granted: Vec<String> = deployed
                    .hosts
                    .grants()
                    .iter()
                    .map(|c| c.to_string())
                    .collect();
                let wanted: Vec<String> = tenant.granted.iter().cloned().collect();
                assert_eq!(granted, wanted, "{}", tenant.name);
            }
            (State::Refused(why), "greedy") => assert!(why.contains("upstream"), "{why}"),
            (State::Refused(why), name) => panic!("{name} was refused: {why}"),
        }
    }

    // What a fresh server answers, which every answer after a failure on the
    // reused worker and isolates has to match.
    let reference = |target: &str| get(fresh.addr, target).unwrap();
    let hello = reference("/hello/?name=again");
    let fine = reference("/trap/ok?n=3");
    let kept = reference("/hog/?n=1000");
    assert_eq!(hello.0, 200);
    assert_eq!(fine, (200, "fine 3\n".to_string()));
    assert_eq!(kept, (200, "kept 1001\n".to_string()));

    let ask = |target: &str| get(server.addr, target).unwrap();
    assert_eq!(ask("/hello/?name=again"), hello);

    // A trap, and the same tenant and another one after it.
    let (status, body) = ask("/trap/?n=0");
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("division by zero"), "{body}");
    assert_eq!(ask("/trap/ok?n=3"), fine);
    assert_eq!(ask("/hello/?name=again"), hello);

    // A run out of its heap.
    let (status, body) = ask("/hog/?n=50000000");
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("this run has no memory left"), "{body}");
    assert_eq!(ask("/hog/?n=1000"), kept);
    assert_eq!(ask("/trap/ok?n=3"), fine);

    // A run stopped by its deadline while running.
    let (status, body) = ask("/hello/spin");
    assert_eq!(status, 504, "{body}");
    assert_eq!(ask("/hello/?name=again"), hello);

    // A run stopped by its deadline while parked at a fetch, whose answer
    // the upstream sends after the run is gone: the late answer reaches no
    // run — not this tenant's next one, on the same worker and isolate.
    let (port, finished) = late_upstream(Duration::from_millis(600));
    let (status, body) = ask(&format!("/proxy/?url=http://127.0.0.1:{port}/slow"));
    assert_eq!(status, 504, "{body}");
    finished
        .recv_timeout(Duration::from_secs(10))
        .expect("the upstream got to its late answer");
    let target = format!("http://127.0.0.1:{}/hello/?name=again", fresh.addr.port());
    let (status, body) = ask(&format!("/proxy/?url={target}"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, format!("{target} said:\n{}", hello.1));
    assert_eq!(stat(&server, "parked"), 0, "nothing is left parked");
    assert_eq!(stat(&server, "timeouts"), 2);

    // Two tenants running the same code over the same kind of host keep
    // their own state, with a failure of a third tenant between them.
    assert!(ask("/counterA/page").1.contains("visited 1 time"));
    assert!(ask("/counterA/page").1.contains("visited 2 time"));
    assert_eq!(ask("/trap/?n=0").0, 500);
    assert!(
        ask("/counterB/page").1.contains("visited 1 time"),
        "counterB sees nothing of counterA's"
    );
    assert!(ask("/counterA/page").1.contains("visited 3 time"));

    let _ = std::fs::remove_dir_all(&root);
}
