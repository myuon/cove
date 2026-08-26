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
//! says so rather than pinning whichever answer this machine happens to give,
//! which is the line ADR 0008's amendment draws.

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
use cove_runtime::process::{Process, ProcessLog};
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
    /// The fake filesystem as the run left it, by path.
    ///
    /// A program that was told to write a file says on the console that it
    /// did, and the console line is not the file. This is what lets a test
    /// assert on what actually landed.
    files: BTreeMap<String, String>,
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
    let files = Files::in_memory(fakes.files);
    let tree = files.tree();

    // Every module is registered whether or not this entry needs it, exactly
    // as `cove run` and `cove test` register them: the grants are what decide,
    // so a capability the program reaches for without being granted is
    // refused with the reason rather than with a missing module.
    let mut hosts = HostRegistry::new(Grants::new(allow.to_vec()));
    hosts.register(Box::new(Console::new(console.clone())));
    hosts.register(Box::new(Env::new(fakes.env)));
    hosts.register(Box::new(Documents::in_memory(fakes.documents)));
    hosts.register(Box::new(files));
    hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    hosts.register(Box::new(Database::recorded(fakes.rows)));
    hosts.register(Box::new(http));
    // An entry that takes its arguments as a parameter is handed them below;
    // one that reads them through `process.args` needs the same list here, so
    // the two ways of asking are the same list either way.
    hosts.register(Box::new(Process::recorded(
        fakes.args.clone(),
        BTreeMap::new(),
        ProcessLog::new(),
    )));

    let args: Vec<Rc<str>> = fakes.args.iter().map(|arg| arg.as_str().into()).collect();
    let runtime = Runtime::new(resolved, sources, Arc::new(hosts));
    let value = Interpreter::new(&runtime)
        .run_entry(module, name, args)
        .unwrap_or_else(|error| panic!("`{entry}` ran without a runtime error: {}", error.message));
    Ran {
        value,
        console: console.lines(),
        served: served.responses(),
        files: tree.files(),
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
/// Measured on one machine, the same program fired the timer in 200 of 200
/// runs with two requests to serve first — 150 of them with every core
/// saturated — and in 0 of 100 with none: what changed was only how much work
/// `main` did before it cancelled. CI, which is slower and more contended,
/// found the zero.
///
/// That is a property of the program rather than of the fake, and it is the
/// same property under a real clock, where a sixty-second timer cancelled
/// after two requests fires zero times. ADR 0008's amendment decides that it
/// stays that way — a `spawn` starts a task and orders nothing else — and
/// records what was rejected for making the count decidable here: a rendezvous
/// at `spawn`, a clock this test steps, and a `clock.every` that reports its
/// rounds. So this asserts what is actually decided: the fake offers one round
/// at most, so at most one line may appear, and a line that does appear is one
/// of the three the program could print. Which of the three is the scheduler's
/// as well — those 200 runs reported one request recorded in 185 of them and
/// none in 15, and saturating the cores reversed the split. `clock.every`'s
/// own behaviour — one round on a virtual clock, an `Err` handed back rather
/// than retried, nothing run at all when the task is already cancelled — is
/// pinned exactly by the unit tests in `crates/cove-runtime/src/clock.rs`,
/// which drive it directly and have no second thread to race.
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
    // A round reports a prefix of the two requests this run serves, because it
    // reads the metrics under the same lock the middleware records them under,
    // and neither of these routes fails. Which prefix it saw belongs to the
    // scheduler; that it saw one of them belongs to the program.
    for line in &timer {
        assert!(
            [
                "requests=0 failures=0",
                "requests=1 failures=0",
                "requests=2 failures=0",
            ]
            .contains(&line.as_str()),
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

// ------------------------------------------------------------------------ cq
//
// `cq` is the one program here big enough to have inputs of its own, and they
// are checked in at `examples/cq/data/`. The tests below hand the fakes those
// same bytes through `include_str!` rather than a copy, so a fixture edited on
// disk and an assertion written here cannot drift apart without one of these
// tests saying so.

/// The bookings `examples/cq/data/bookings.jsonl` holds, as the fake
/// filesystem's contents under the name the command line gives it.
const BOOKINGS: &str = include_str!("../../../examples/cq/data/bookings.jsonl");

/// The same bookings with three records this program refuses in among them.
const MALFORMED: &str = include_str!("../../../examples/cq/data/bookings-malformed.jsonl");

/// The seasonal rate card `rate-card` reads.
const RATES: &str = include_str!("../../../examples/cq/data/rates.csv");

/// One fake filesystem holding `contents` at `path`.
fn file(path: &str, contents: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(path.to_string(), contents.to_string())])
}

/// Runs `cq.main` with `args` over a filesystem holding `files`.
fn cq(args: &[&str], files: BTreeMap<String, String>) -> Ran {
    run(
        "cq.main",
        &["console", "files", "process"],
        Fakes {
            args: args.iter().map(|arg| arg.to_string()).collect(),
            files,
            ..Fakes::default()
        },
    )
}

/// The message a Cove entry that answered `Err` carried.
fn err(value: &Value) -> String {
    let Value::Enum(result) = value else {
        panic!("expected a `Result`, found {value}");
    };
    assert_eq!(&*result.case, "Err", "expected `Err(...)`, found {value}");
    result.payload[0].to_string()
}

/// `revenue-summary` groups the bookings it read by property, in ascending
/// order by name, and says on the console what the run did.
///
/// The order is the summary's own `Map` order rather than the input's, which
/// is what makes this a golden assertion rather than a set comparison: two
/// runs over the same file write the same bytes.
#[test]
fn cq_summarizes_revenue_by_property() {
    let ran = cq(
        &["bookings.jsonl", "--program", "revenue-summary"],
        file("bookings.jsonl", BOOKINGS),
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        [
            "property,bookings,nights,revenue,averageNightlyRate",
            "harbour-loft,8,12,2208.00,184.00",
            "orchard-barn,6,34,3272.50,96.25",
            "seaside-cottage,8,28,3626.00,129.50",
            "cq: read 24 records, wrote 3 rows to the console",
        ],
        "{:?}",
        ran.console
    );
}

/// `rate-card` reads a CSV and writes JSON Lines, which is the crossing this
/// program exists to show: the header is consumed rather than written out,
/// each rate becomes a number, and a note holding a comma or a quote survives
/// the change of format.
#[test]
fn cq_normalizes_the_rate_card_into_json_lines() {
    let ran = cq(
        &["rates.csv", "--program", "rate-card"],
        file("rates.csv", RATES),
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        [
            r#"{"nightlyRate":109,"notes":"Two bedrooms, sea view","property":"seaside-cottage","season":"low"}"#,
            r#"{"nightlyRate":159,"notes":"Minimum stay 3 nights","property":"seaside-cottage","season":"high"}"#,
            r#"{"nightlyRate":164,"notes":"","property":"harbour-loft","season":"low"}"#,
            r#"{"nightlyRate":219,"notes":"Says \"quiet\" on the listing","property":"harbour-loft","season":"high"}"#,
            r#"{"nightlyRate":86.5,"notes":"Dog friendly","property":"orchard-barn","season":"low"}"#,
            r#"{"nightlyRate":124,"notes":"Closed for repairs, 12–14 March","property":"orchard-barn","season":"high"}"#,
            "cq: read 7 records, wrote 6 rows to the console",
        ],
        "{:?}",
        ran.console
    );
}

/// `--limit` stops the run after that many records, so the rows are the ones
/// the first three bookings produce and the report says three were read.
#[test]
fn cq_stops_after_the_limit_it_was_given() {
    let ran = cq(
        &[
            "bookings.jsonl",
            "--program",
            "confirmed-bookings",
            "--limit",
            "3",
        ],
        file("bookings.jsonl", BOOKINGS),
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        [
            r#"{"checkIn":"2026-03-01","guest":"Ada Lovelace","id":"B-0001","nights":3,"property":"seaside-cottage","revenue":388.5}"#,
            r#"{"checkIn":"2026-03-02","guest":"Grace Hopper","id":"B-0002","nights":2,"property":"harbour-loft","revenue":368}"#,
            r#"{"checkIn":"2026-03-03","guest":"Alan Turing","id":"B-0003","nights":5,"property":"orchard-barn","revenue":481.25}"#,
            "cq: read 3 records, wrote 3 rows to the console",
        ],
        "{:?}",
        ran.console
    );
}

/// `--limit` bounds the records taken from the input, sound or not, so a run
/// over a file with a bad record among the first three takes those three and
/// stops: two read, one skipped.
///
/// The meaning was chosen over counting only the records that were good. A
/// limit is a bound on the work the run does, and counting the good ones would
/// make how much of the file was touched depend on how much of it was wrong —
/// `--limit 3` over a file whose first hundred lines are malformed would read a
/// hundred and three of them. That is a bound the person who wrote `3` cannot
/// predict, so the count is of records taken.
#[test]
fn cq_counts_the_records_it_took_against_the_limit_not_the_good_ones() {
    let ran = cq(
        &[
            "bookings-malformed.jsonl",
            "--program",
            "confirmed-bookings",
            "--skip-invalid",
            "--limit",
            "3",
        ],
        file("bookings-malformed.jsonl", MALFORMED),
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        [
            r#"{"checkIn":"2026-03-01","guest":"Ada Lovelace","id":"B-0001","nights":3,"property":"seaside-cottage","revenue":388.5}"#,
            r#"{"checkIn":"2026-03-02","guest":"Grace Hopper","id":"B-0002","nights":2,"property":"harbour-loft","revenue":368}"#,
            "bookings-malformed.jsonl:3:1: `nights` must be a number, and is a string",
            "cq: read 2 records and skipped 1, wrote 2 rows to the console",
        ],
        "{:?}",
        ran.console
    );
}

/// A record this program cannot read stops the run, and the failure it
/// answers is the `file:line:column: message` an editor can jump to.
///
/// Stopping is the default because a summary computed from most of a file is
/// a wrong answer that looks like a right one. The header has already been
/// written by then, which is what a streaming program does and is why the
/// console is asserted too.
#[test]
fn cq_stops_at_the_first_record_it_cannot_read() {
    let ran = cq(
        &["bookings-malformed.jsonl", "--program", "revenue-summary"],
        file("bookings-malformed.jsonl", MALFORMED),
    );

    assert_eq!(
        err(&ran.value),
        "bookings-malformed.jsonl:3:1: `nights` must be a number, and is a string"
    );
    assert_eq!(
        ran.console,
        ["property,bookings,nights,revenue,averageNightlyRate"],
        "{:?}",
        ran.console
    );
}

/// `--skip-invalid` reports each bad record where it stands and keeps going,
/// so the same file yields three diagnostics, a summary over the records that
/// were good, and a report that counts the three it skipped.
///
/// The three are one per kind of failure the file holds: a field of the wrong
/// type, a line that is not JSON at all, and a record missing a field. The
/// blank line in that file is none of them — it is skipped without being
/// counted, which is why the report says seven read rather than eight.
#[test]
fn cq_reports_and_skips_the_records_it_cannot_read() {
    let ran = cq(
        &[
            "bookings-malformed.jsonl",
            "--program",
            "revenue-summary",
            "--skip-invalid",
        ],
        file("bookings-malformed.jsonl", MALFORMED),
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        [
            "property,bookings,nights,revenue,averageNightlyRate",
            "bookings-malformed.jsonl:3:1: `nights` must be a number, and is a string",
            "bookings-malformed.jsonl:6:35: expected `,` or `}` after a field",
            "bookings-malformed.jsonl:9:1: this record has no `id` field",
            "harbour-loft,2,3,552.00,184.00",
            "orchard-barn,1,5,481.25,96.25",
            "seaside-cottage,3,10,1295.00,129.50",
            "cq: read 7 records and skipped 3, wrote 3 rows to the console",
        ],
        "{:?}",
        ran.console
    );
}

/// `--output` sends the rows to a file and leaves only the report on the
/// console, so what has to be asserted is what landed in the filesystem.
///
/// The console line saying a file was written is not the file, and a program
/// that printed it without writing anything would pass a test that read only
/// the console.
#[test]
fn cq_writes_to_the_file_it_was_given_rather_than_the_console() {
    let ran = cq(
        &[
            "bookings.jsonl",
            "--program",
            "revenue-summary",
            "--output",
            "summary.csv",
            "--limit",
            "4",
        ],
        file("bookings.jsonl", BOOKINGS),
    );

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        ["cq: read 4 records, wrote 3 rows to summary.csv"],
        "{:?}",
        ran.console
    );
    assert_eq!(
        ran.files.get("summary.csv").map(String::as_str),
        Some(concat!(
            "property,bookings,nights,revenue,averageNightlyRate\n",
            "harbour-loft,2,3,552.00,184.00\n",
            "orchard-barn,1,5,481.25,96.25\n",
            "seaside-cottage,1,3,388.50,129.50\n",
        )),
        "{:?}",
        ran.files
    );
    // The input is still there, unchanged: a run that reads one file and
    // writes another must not have touched the one it read.
    assert_eq!(
        ran.files.get("bookings.jsonl").map(String::as_str),
        Some(BOOKINGS)
    );
}

/// `--help` prints the usage text and reads nothing, and the text names every
/// program the package declares rather than a list kept beside them.
#[test]
fn cq_prints_its_usage_when_asked_for_help() {
    let ran = cq(&["--help"], BTreeMap::new());

    assert!(ok(&ran.value).eq_value(&Value::Unit), "{}", ran.value);
    assert_eq!(
        ran.console,
        [
            "cq -- a typed streaming transformation over JSON Lines and CSV",
            "",
            "usage: cq <input> --program <name> [options]",
            "",
            "programs:",
            "  revenue-summary     group bookings by property and total their nights and revenue (reads jsonl)",
            "  confirmed-bookings  keep the confirmed bookings and report what each one is worth (reads jsonl)",
            "  rate-card           read a seasonal rate card and normalize it (reads csv)",
            "",
            "options:",
            "  --input <path>          the file to read, if not given first",
            "  --output <path>         write here instead of the console",
            "  --output-format <name>  `jsonl` or `csv`; each program has a default",
            "  --limit <count>         stop after taking this many records, sound or not",
            "  --skip-invalid          report a bad record and keep going",
            "  --help                  this text",
        ],
        "{:?}",
        ran.console
    );
}

/// `cq.sample` writes records `cq.main` can read, which is the one thing a
/// generator has to get right.
///
/// It builds its JSON by interpolation rather than through `cq.json`, so
/// nothing but this makes the two agree: a generator whose output its own
/// reader refuses is the failure most worth catching, and it would not show
/// up in either half's own tests. The file the first run left behind is
/// handed to the second exactly as it stands, so what is asserted is that the
/// bytes crossed.
#[test]
fn cq_reads_back_the_sample_it_generated() {
    let generated = run(
        "cq.sample",
        &["console", "files", "process"],
        Fakes {
            args: vec!["4".to_string(), "sample.jsonl".to_string()],
            ..Fakes::default()
        },
    );

    assert!(
        ok(&generated.value).eq_value(&Value::Unit),
        "{}",
        generated.value
    );
    assert_eq!(generated.console, ["cq: wrote 4 records to sample.jsonl"]);
    assert_eq!(
        generated.files.get("sample.jsonl").map(String::as_str),
        Some(concat!(
            r#"{"id":"B-000001","guest":"Donald Knuth","property":"seaside-cottage","checkIn":"2026-03-19","nights":5,"guests":3,"rate":129.5,"status":"confirmed","channel":"agency"}"#,
            "\n",
            r#"{"id":"B-000002","guest":"Donald Knuth","property":"seaside-cottage","checkIn":"2026-03-20","nights":6,"guests":2,"rate":129.5,"status":"pending","channel":"direct"}"#,
            "\n",
            r#"{"id":"B-000003","guest":"Alan Turing","property":"harbour-loft","checkIn":"2026-03-21","nights":7,"guests":2,"rate":184.0,"status":"confirmed","channel":"direct"}"#,
            "\n",
            r#"{"id":"B-000004","guest":"Frances Allen","property":"harbour-loft","checkIn":"2026-03-26","nights":5,"guests":1,"rate":184.0,"status":"confirmed","channel":"agency"}"#,
            "\n",
        )),
        "{:?}",
        generated.files
    );

    let read = cq(
        &["sample.jsonl", "--program", "confirmed-bookings"],
        generated.files,
    );

    assert!(ok(&read.value).eq_value(&Value::Unit), "{}", read.value);
    assert_eq!(
        read.console,
        [
            r#"{"checkIn":"2026-03-19","guest":"Donald Knuth","id":"B-000001","nights":5,"property":"seaside-cottage","revenue":647.5}"#,
            r#"{"checkIn":"2026-03-21","guest":"Alan Turing","id":"B-000003","nights":7,"property":"harbour-loft","revenue":1288}"#,
            r#"{"checkIn":"2026-03-26","guest":"Frances Allen","id":"B-000004","nights":5,"property":"harbour-loft","revenue":920}"#,
            "cq: read 4 records, wrote 3 rows to the console",
        ],
        "{:?}",
        read.console
    );
}
