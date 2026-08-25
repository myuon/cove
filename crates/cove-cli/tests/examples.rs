//! The representative programs, run against deterministic fake hosts.
//!
//! `tests/e2e` runs programs through the `cove` binary, which installs the
//! real host implementations. That is the right boundary for a program whose
//! hosts can be real and hermetic at once, and the wrong one for a server: a
//! real listener waits for a client that a golden-file test has no way to be,
//! and a real database is something this toolchain does not have at all.
//!
//! So `examples/server`, `examples/tasks`, and `examples/callbacks` are run
//! here instead, against the fakes their capabilities name — a listener with
//! a scripted queue of requests, a `fetch` with recorded answers, a clock
//! that moves only when something moves it, and a database of canned rows.
//! Every one of them is deterministic, which is what lets these be assertions
//! rather than smoke tests.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::host::{Console, Grants, HostRegistry};
use cove_runtime::http::{Http, ScriptedRequest};
use cove_runtime::interp::Interpreter;
use cove_runtime::runtime::Runtime;
use cove_runtime::value::Value;
use cove_sema::resolve::Program;

/// The `examples/` package at the repository root.
fn examples_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// A console a test can read back, shared with the task threads that write
/// to it.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The data the fakes answer from, which is the whole of what a run of one of
/// these programs can observe.
#[derive(Default)]
struct Fakes {
    /// What `http.fetch` answers, by URL.
    bodies: BTreeMap<String, String>,
    /// What a listener hands the program, in order.
    requests: Vec<ScriptedRequest>,
    /// What a connection answers, by query text.
    rows: BTreeMap<String, Vec<String>>,
}

/// What one run produced.
struct Ran {
    value: Value,
    /// Every line the program printed, in the order this run happened to
    /// produce them.
    console: Vec<String>,
    /// Every response the program served, in order, as `<status> <body>`.
    served: Vec<String>,
}

impl Ran {
    /// Whether some line of the console output is exactly `line`.
    fn printed(&self, line: &str) -> bool {
        self.console.iter().any(|printed| printed == line)
    }

    /// Whether some line of the console output starts with `prefix`.
    fn printed_starting(&self, prefix: &str) -> bool {
        self.console
            .iter()
            .any(|printed| printed.starts_with(prefix))
    }
}

fn program() -> (Arc<SourceMap>, Arc<Program>) {
    let mut sources = SourceMap::new();
    let package = cove_sema::package::load(&examples_root(), &mut sources)
        .expect("the examples package loads");
    let program = cove_sema::resolve::resolve(&package).expect("the examples package resolves");
    (Arc::new(sources), Arc::new(program))
}

/// Runs one entry of the examples package against fakes, granting `allow`.
fn run(entry: &str, allow: &[&str], fakes: Fakes) -> Ran {
    let (sources, resolved) = program();
    let (module, name) = entry.rsplit_once('.').expect("a qualified entry");

    let console = Buffer::default();
    let http = Http::recorded(fakes.bodies, fakes.requests);
    let served = http.served();

    let mut hosts = HostRegistry::new(Grants::new(allow.to_vec()));
    hosts.register(Box::new(Console::new(console.clone())));
    hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    hosts.register(Box::new(Database::recorded(fakes.rows)));
    hosts.register(Box::new(http));

    let runtime = Runtime::new(resolved, sources, Arc::new(hosts));
    let value = Interpreter::new(&runtime)
        .run_entry(module, name, Vec::<Rc<str>>::new())
        .unwrap_or_else(|error| panic!("`{entry}` ran without a runtime error: {}", error.message));
    Ran {
        value,
        console: console.lines(),
        served: served.responses(),
    }
}

/// `Ok(...)`, or the message an `Err` carried.
fn ok(value: &Value) -> &Value {
    match value {
        Value::Enum(result) if &*result.type_name == "Result" && &*result.case == "Ok" => {
            result.payload.first().expect("`Ok` carries a value")
        }
        other => panic!("expected `Ok(...)`, found {other}"),
    }
}

/// `server` listens, serves what arrives, and stops when nothing more does.
#[test]
fn the_server_answers_its_routes_and_stops_when_the_listener_is_empty() {
    let ran = run(
        "server.main",
        &["http", "console"],
        Fakes {
            requests: vec![
                ScriptedRequest::get("/health"),
                ScriptedRequest::get("/missing"),
            ],
            ..Fakes::default()
        },
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.served,
        [
            "200 {\"status\":\"ok\"}",
            "404 \"no route for Get /missing\"",
        ]
    );
    assert_eq!(
        ran.console,
        ["listening on :8080", "served 2 request(s)"],
        "{:?}",
        ran.console
    );
}

/// `tasks` fetches both inputs concurrently and finishes inside its bound.
#[test]
fn the_dashboard_loads_both_inputs_within_its_timeout() {
    let ran = run(
        "tasks.loadDashboard",
        &["http", "clock"],
        Fakes {
            bodies: BTreeMap::from([
                (
                    "http://127.0.0.1:8080/bookings".to_string(),
                    "[\"b-1\"]".to_string(),
                ),
                (
                    "http://127.0.0.1:8080/prices".to_string(),
                    "[\"p-1\"]".to_string(),
                ),
            ]),
            ..Fakes::default()
        },
    );

    let Value::Struct(dashboard) = ok(&ran.value) else {
        panic!("expected a `Dashboard`, found {}", ran.value);
    };
    assert_eq!(&*dashboard.type_name, "tasks.Dashboard");
    assert_eq!(
        dashboard.get("bookings").map(ToString::to_string),
        Some("[\"b-1\"]".to_string())
    );
    assert_eq!(
        dashboard.get("prices").map(ToString::to_string),
        Some("[\"p-1\"]".to_string())
    );
}

/// A fetch the fake has no answer for fails the whole load, since the
/// dashboard needs both.
#[test]
fn the_dashboard_reports_a_fetch_the_host_could_not_answer() {
    let ran = run("tasks.loadDashboard", &["http", "clock"], Fakes::default());

    let Value::Enum(result) = &ran.value else {
        panic!("expected a `Result`, found {}", ran.value);
    };
    assert_eq!(&*result.case, "Err");
    assert!(
        result.payload[0]
            .to_string()
            .starts_with("http: no recorded answer for"),
        "{}",
        result.payload[0]
    );
}

/// `callbacks` opens a connection, serves both routes through its middleware,
/// and closes what it opened.
///
/// The console is asserted line by line rather than as a whole: the repeating
/// timer runs in a task of its own, so where its line lands among the
/// request lines is the scheduler's business and not the program's.
#[test]
fn the_callback_server_serves_both_routes_through_its_middleware() {
    let ran = run(
        "callbacks.main",
        &["http", "database", "clock", "console"],
        Fakes {
            requests: vec![
                ScriptedRequest::get("/health"),
                ScriptedRequest::post("/bookings", "b-1"),
            ],
            rows: BTreeMap::from([(
                "insert into bookings values (b-1)".to_string(),
                vec!["b-1".to_string()],
            )]),
            ..Fakes::default()
        },
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.served,
        ["200 {\"status\":\"ok\"}", "201 {\"id\":\"b-1\"}"]
    );
    assert!(ran.printed("listening on :8080"), "{:?}", ran.console);
    assert!(ran.printed("Get /health"), "{:?}", ran.console);
    assert!(ran.printed("Post /bookings"), "{:?}", ran.console);
    assert!(
        ran.printed("event: BookingCreated(b-1)"),
        "{:?}",
        ran.console
    );
    // The timer fired once: a virtual clock has no time of its own, so one
    // round is all a repeating timer gets from it.
    assert!(ran.printed_starting("requests="), "{:?}", ran.console);
}
