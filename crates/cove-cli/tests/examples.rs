//! Every representative program, run against deterministic fake hosts.
//!
//! `examples/README.md` calls these programs executable design tests, and a
//! design test nobody runs is a design document. `tests/e2e` runs programs
//! through the `cove` binary, which installs the real host implementations;
//! that is the right boundary for a program whose hosts can be real and
//! hermetic at once, and the wrong one for a server — a real listener waits
//! for a client a golden-file test has no way to be — and impossible for a
//! database this toolchain does not have.
//!
//! So every one of them is run here, against the fakes its capabilities name:
//! a console that is a buffer, an environment the test writes, a fixed set of
//! documents and files, a listener with a scripted queue of requests, a
//! `fetch` with recorded answers, a clock that moves only when something
//! moves it, and a database of canned rows. Cove code cannot tell any of them
//! from the real thing, and every one of them is deterministic, which is what
//! lets these be assertions rather than smoke tests.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::files::Files;
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
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
    /// The arguments the entry function receives.
    args: Vec<String>,
    /// What `env.get` answers.
    env: BTreeMap<String, String>,
    /// What `documents.read` answers, by document name.
    documents: BTreeMap<String, String>,
    /// What `files.read` answers, by path.
    files: BTreeMap<String, String>,
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

    // Every module is registered whether or not this entry needs it, exactly
    // as `cove run` and `cove test` register them: the grants are what decide,
    // so a capability the program reaches for without being granted is
    // refused with the reason rather than with a missing module.
    let mut hosts = HostRegistry::new(Grants::new(allow.to_vec()));
    hosts.register(Box::new(Console::new(console.clone())));
    hosts.register(Box::new(Env::new(fakes.env)));
    hosts.register(Box::new(Documents::in_memory(fakes.documents)));
    hosts.register(Box::new(Files::in_memory(fakes.files)));
    hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    hosts.register(Box::new(Database::recorded(fakes.rows)));
    hosts.register(Box::new(http));

    let args: Vec<Rc<str>> = fakes.args.iter().map(|arg| arg.as_str().into()).collect();
    let runtime = Runtime::new(resolved, sources, Arc::new(hosts));
    let value = Interpreter::new(&runtime)
        .run_entry(module, name, args)
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

/// `hello` greets whoever it was given, and the world when it was given
/// nobody.
#[test]
fn hello_greets_its_argument_or_the_world() {
    let named = run(
        "hello.main",
        &["console"],
        Fakes {
            args: vec!["Cove".to_string()],
            ..Fakes::default()
        },
    );
    assert_eq!(named.console, ["Hello, Cove!"]);

    let bare = run("hello.main", &["console"], Fakes::default());
    assert_eq!(bare.console, ["Hello, world!"]);
}

/// `config` reads the environment the host chose to expose, and reports a
/// value it cannot validate as an ordinary `Err`.
#[test]
fn config_validates_what_the_environment_supplied() {
    let ran = run(
        "config.loadConfig",
        &["env"],
        Fakes {
            env: BTreeMap::from([
                ("PORT".to_string(), "9000".to_string()),
                ("LOG_LEVEL".to_string(), "warn".to_string()),
            ]),
            ..Fakes::default()
        },
    );
    let Value::Struct(config) = ok(&ran.value) else {
        panic!("expected a `Config`, found {}", ran.value);
    };
    assert_eq!(
        config.get("port").map(ToString::to_string),
        Some("9000".to_string())
    );
    assert_eq!(
        config.get("logLevel").map(ToString::to_string),
        Some("Warn".to_string())
    );

    // An environment that says nothing is the documented default, and one
    // that says something unusable is a failure the program reports.
    let defaulted = run("config.loadConfig", &["env"], Fakes::default());
    let Value::Struct(config) = ok(&defaulted.value) else {
        panic!("expected a `Config`, found {}", defaulted.value);
    };
    assert_eq!(
        config.get("port").map(ToString::to_string),
        Some("8080".to_string())
    );

    let refused = run(
        "config.loadConfig",
        &["env"],
        Fakes {
            env: BTreeMap::from([("PORT".to_string(), "eighty".to_string())]),
            ..Fakes::default()
        },
    );
    let Value::Enum(result) = &refused.value else {
        panic!("expected a `Result`, found {}", refused.value);
    };
    assert_eq!(&*result.case, "Err");
}

/// `values` prints what copying, aliasing, and freezing actually do: a
/// struct's scalar field copies while its vector field shares storage, so the
/// copy's status stays behind while both handles see the same length.
#[test]
fn values_reports_its_own_collection_lifecycle() {
    let ran = run("values.main", &["console"], Fakes::default());

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        ["Pending", "Confirmed", "2", "2", "2", "1"],
        "{:?}",
        ran.console
    );
}

/// `traits` resolves a bound at the call site and a `dyn` value from what it
/// carries, and prints a report built both ways.
#[test]
fn traits_reports_both_forms_of_dispatch() {
    let ran = run("traits.main", &["console"], Fakes::default());

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        [
            "Latest: booking 41 for 2 guest(s)",
            "Latest: receipt for booking 41: 12500c",
            "Report",
            "- booking 41 for 2 guest(s)",
            "  $ receipt for booking 41: 12500c",
        ],
        "{:?}",
        ran.console
    );
}

/// `restricted` reaches the console only through `text.report`, and reaches
/// the document through the one capability the host granted.
#[test]
fn restricted_reads_only_the_document_the_host_named() {
    let ran = run(
        "restricted.main",
        &["documents", "console"],
        Fakes {
            documents: BTreeMap::from([(
                "input".to_string(),
                "Cove grants only narrow authority.".to_string(),
            )]),
            ..Fakes::default()
        },
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(ran.console, ["5 words"]);
}

/// `codegen` derives a module from a data file, which is what
/// `cove generate --check` keeps honest on disk. Here it runs against a file
/// the test wrote, so what is asserted is the generator and not the checkout.
#[test]
fn codegen_derives_a_module_from_the_file_it_was_given() {
    let ran = run(
        "codegen.statusCodes",
        &["files"],
        Fakes {
            files: BTreeMap::from([(
                "status_codes.txt".to_string(),
                "200 Ok\n404 NotFound\n".to_string(),
            )]),
            ..Fakes::default()
        },
    );

    let generated = ok(&ran.value).to_string();
    assert!(generated.contains("enum StatusCode"), "{generated}");
    assert!(generated.contains("Ok"), "{generated}");
    assert!(generated.contains("NotFound"), "{generated}");
    assert!(generated.contains("404"), "{generated}");
}
