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
//!
//! A fake being deterministic does not make a *program* deterministic. Three
//! of these programs spawn tasks, ADR 0008 gives each one a thread, and the
//! order in which two threads reach the console is the scheduler's. So an
//! assertion here has to be about something the program decides: the order
//! within one task, the set of effects a scope produced, the value an entry
//! returned. Where the program decides nothing — most sharply, how many times
//! a repeating timer fires before its own cancellation reaches it — the test
//! says so rather than pinning whichever answer this machine happens to give.

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
    /// The console output with the lines starting `prefix` taken out, and
    /// those lines returned beside it.
    ///
    /// Every task of a run prints to one console, so what a test reads back is
    /// one interleaving of several tasks' output and which interleaving it is
    /// belongs to the scheduler. Separating the lines by who printed them is
    /// what lets an assertion pin the order *within* a task, which the program
    /// decides, without pinning an order *between* tasks, which it does not.
    fn split_off(&self, prefix: &str) -> (Vec<String>, Vec<String>) {
        self.console
            .iter()
            .cloned()
            .partition(|line| !line.starts_with(prefix))
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
/// dashboard needs both — and the failure it reports is the one the scope
/// surfaced, not the one the body tripped over.
///
/// Both fetches fail here. The body awaits `bookings` first, so its `?` ends
/// the block and `prices` is never awaited at all; leaving the scope waits for
/// it anyway, finds it settled on an `Err`, and returns *that* from the
/// enclosing function, exactly as ADR 0008 says a scope does with a child
/// whose failure nobody collected. So the answer names `/prices`, and it does
/// so every time: which task goes unawaited is decided by the order the
/// initializer's arguments are written in, not by which thread finished
/// first.
#[test]
fn the_dashboard_reports_a_fetch_the_host_could_not_answer() {
    let ran = run("tasks.loadDashboard", &["http", "clock"], Fakes::default());

    let Value::Enum(result) = &ran.value else {
        panic!("expected a `Result`, found {}", ran.value);
    };
    assert_eq!(&*result.case, "Err");
    assert_eq!(
        result.payload[0].to_string(),
        "http: no recorded answer for `http://127.0.0.1:8080/prices`"
    );
}

/// A failure the body *did* await is the body's own value, and the scope has
/// nothing left to surface over it.
///
/// This is the same program as above with one fetch answered, which is what
/// makes the pair readable: the only difference is whether a task was left
/// unawaited, and that is what decides which failure comes back.
#[test]
fn the_dashboard_reports_an_awaited_failure_as_the_bodys_own_value() {
    let ran = run(
        "tasks.loadDashboard",
        &["http", "clock"],
        Fakes {
            bodies: BTreeMap::from([(
                "http://127.0.0.1:8080/prices".to_string(),
                "[\"p-1\"]".to_string(),
            )]),
            ..Fakes::default()
        },
    );

    let Value::Enum(result) = ok(&ran.value) else {
        panic!("expected a `Result`, found {}", ran.value);
    };
    assert_eq!(&*result.case, "Err");
    assert_eq!(
        result.payload[0].to_string(),
        "http: no recorded answer for `http://127.0.0.1:8080/bookings`"
    );
}

/// `callbacks` opens a connection, serves both routes through its middleware,
/// emits an event, and closes what it opened.
///
/// Everything the request-serving task prints is asserted in order, because
/// that task is what decides that order: the middleware prints after the
/// handler it wraps has returned, so the event line precedes the `Post` line.
///
/// The report timer is a different matter, and the reason this comment is
/// long. `main` spawns it and cancels it once the listener runs dry, so
/// whether it fires at all is a race between `reportTimer.cancel()` and the
/// operating system getting round to starting the timer's thread —
/// `clock.every` reads its task's cancellation flag before it does anything
/// else, so a `cancel` that lands first means no round at all. A virtual clock
/// does not settle that race, because the clock is not what decides it.
/// Measured on one machine, the same program fired the timer in 60 of 60 runs
/// with two requests to serve first and in 0 of 40 with none: what changed was
/// only how much work `main` did before it cancelled. CI, which is slower and
/// more contended, found the zero.
///
/// That is a property of the program rather than of the fake, and it is the
/// same property under a real clock, where a sixty-second timer cancelled
/// after two requests fires zero times. So this asserts what is actually
/// decided: the fake offers one round at most, so at most one line may appear,
/// and if one does it is the line the program prints. `clock.every`'s own
/// behaviour — one round on a virtual clock, an `Err` handed back rather than
/// retried, nothing run at all when the task is already cancelled — is pinned
/// exactly by the unit tests in `crates/cove-runtime/src/clock.rs`, which
/// drive it directly and have no second thread to race. Issue #39 records what
/// would have to change for the count to be decidable here too.
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

    let (serving, timer) = ran.split_off("requests=");
    assert_eq!(
        serving,
        [
            "listening on :8080",
            "Get /health",
            "event: BookingCreated(b-1)",
            "Post /bookings",
        ],
        "{:?}",
        ran.console
    );

    assert!(
        timer.len() <= 1,
        "a virtual clock gives a repeating timer one round at most: {timer:?}"
    );
    for line in &timer {
        assert!(
            line.starts_with("requests=") && line.contains(" failures="),
            "{line}"
        );
    }
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
