//! `http`: fetching over the network, and listening on a port.
//!
//! The Language Card lists the network among the operations that are typed
//! Host APIs rather than ambient authority, and `examples/server/main.cove`
//! shows the shape it expects:
//!
//! ```cove
//! let server = http.listen(8080)?
//! while server.handle(routes)? {
//! }
//! server.close()?
//! ```
//!
//! Two things there are not ordinary host calls. `server` is a resource
//! handle: `listen` hands back a name for a socket the host keeps, and later
//! calls are made on that name rather than on the module. And `handle` is
//! given a routing table whose entries hold Cove closures, which it has to
//! *run*. ADR 0013 is what makes both possible —
//! [`crate::host::ResourceHandle`] for the first and [`crate::host::Reentry`]
//! for the second.
//!
//! The loop belongs to the program rather than to the host. A `serve` that
//! never returned would be a host call outside the reach of the run's fuel,
//! its deadline, and its cancellation; `handle` answers one request and
//! returns, so the loop around it is ordinary Cove code with ordinary
//! safepoints.
//!
//! That is only half of it, because `handle` itself waits — for a connection,
//! and then for the request on it — and a host call is a hole in the
//! safepoint chain for as long as it lasts. So the waiting is bounded the way
//! [`crate::host::HostApi`] says a blocking operation must bound it. A real
//! listener accepts by polling a nonblocking socket, looks at the run's
//! cancellation and at what is left of its deadline between polls, and
//! answers "nothing more to serve" when either says to stop, which ends the
//! program's own loop and lets the run stop at its next safepoint with the
//! diagnostic the budget owns. One request gets one deadline covering its
//! line, its headers, and its body together, no longer than what the run has
//! left. Stopping the run stops the server, and now that is true while it is
//! idle as well as while it is busy.
//!
//! Three implementations ship. [`Http::real`] speaks HTTP/1.1 over TCP, and
//! is deliberately small: one request per connection, `Connection: close`, no
//! keep-alive, no chunked transfer, and a listener that binds loopback only,
//! because granting `http` should not publish a port to the network the
//! machine is on.
//!
//! It is small in what it will hold, too, and a reader should know where the
//! lines are before finding one. A request line longer than eight kibibytes
//! is answered `414`; a header line longer than eight kibibytes, more than a
//! hundred headers, or more than thirty-two kibibytes of them together are
//! answered `431`; a body over one mebibyte is answered `413`. Each bound is
//! applied while the request is being read rather than after, so what a peer
//! claims never decides what this host allocates. A `Content-Length` that is
//! not a plain count of bytes is answered `400`, as are two that disagree,
//! and a `Transfer-Encoding` of any kind is answered `501`, because a body
//! whose end this host cannot find is one it will not start. The bounds are
//! constants, not configuration: this host answers JSON on loopback, and
//! nothing about that job is served by letting a peer choose how much of this
//! process it occupies.
//!
//! The client is bounded on the same argument and by the same number. A
//! response body over one mebibyte is an error rather than an allocation, and
//! `MAX_RESPONSE_BYTES` is where that is said — a server this host reaches
//! is no more this process's to trust than a peer that connects to it, and
//! [ADR 0018](../../../docs/adr/0018-streaming-file-io.md) already settled
//! that a host reads what it decided to read rather than what the input asked
//! it to.
//!
//! What the client answers is an `http.Response`: the status the server sent
//! and the body it carried. A status outside 200-299 is an answer and not a
//! failure, so a program can tell a `404` it received from a connection it
//! could not make, which is what a `Result<String, Error>` could only say in
//! prose. [`Http::recorded`] is the fake: `fetch` answers from a table of
//! canned responses and a listener replays a scripted queue of requests, so a
//! program that serves is testable without a socket. [`Http::denied`] refuses
//! everything and says why.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_schema::builtins::RESULT;

use crate::error::RuntimeError;
use crate::host::{HostApi, Reentry, ResourceHandle};
use crate::schema::ModuleSchema;
use crate::value::{EnumValue, StructValue, Value};

/// How long the real host is willing to spend reading one whole request.
///
/// This is the allowance for the request line, the headers, and the body
/// together, not for each read that makes them up: a connection that opens
/// and dribbles a byte at a time would otherwise hold `handle` open for as
/// long as the peer chose to keep it. A run with a deadline shortens it
/// further, since waiting past the run's own end serves nobody.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the real host sleeps between looks at a listener nobody has
/// connected to yet.
///
/// The same reasoning as `clock`'s own `WATCH_INTERVAL`, and the same value:
/// this is a granularity, so it decides only how long past a cancellation or
/// a deadline the wait may run before it notices. Sleeping is what keeps the
/// poll a poll rather than a spin — a few hundred wakeups a second cost
/// nothing measurable, and a loop with no sleep in it would burn a core to
/// learn the same thing.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// The most of a request line the real host will read.
///
/// Eight kibibytes is the figure the common servers settled on, and it is
/// generous for what it has to hold: a method, a target, and a version. A
/// peer that needs more of a URL than this has a problem this host cannot
/// help with, and a peer sending an endless first line is the case the bound
/// exists for — the read stops here rather than at the end of what the peer
/// felt like sending.
const MAX_REQUEST_LINE: usize = 8 * 1024;

/// The most of one header line the real host will read.
///
/// The same eight kibibytes, for the same reason. One header is a name and a
/// value, and a value that does not fit in eight kibibytes is not a value
/// this host has any use for.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// The most the real host will read of all the header lines together.
///
/// A bound on each line alone bounds nothing: a peer can send a great many
/// short ones. So the lines are counted as they arrive and the total is what
/// stops them, at four full-size headers' worth, which is more than any
/// ordinary client sends and far less than a peer with time on its hands
/// would like to send.
const MAX_HEADERS_BYTES: usize = 32 * 1024;

/// How many header lines the real host will read.
///
/// The byte total already bounds the memory; this bounds the work, since a
/// hundred thousand empty headers cost almost no bytes and still have to be
/// looked at one at a time. A hundred is several times what a browser sends.
const MAX_HEADER_COUNT: usize = 100;

/// The most body the real host will hold.
///
/// One mebibyte, because this host answers JSON on loopback and is not a
/// place to upload a file to. The number is the whole of the promise: a peer
/// cannot make this process hold more than this per request no matter what
/// its `Content-Length` says, since the claim is checked against this bound
/// before a byte of body is read and the bytes are collected as they arrive
/// rather than reserved against the claim.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The most of a response the real client will hold.
///
/// The same mebibyte as [`MAX_BODY_BYTES`], because it is the same promise
/// read from the other side: this host will not let something at the other
/// end of a socket decide how much of this process it occupies, and which end
/// opened the connection does not change that. A server a program chose to
/// fetch from is not more trustworthy than a peer that connected to the
/// listener — a URL in a manifest reaches whatever is answering on that port
/// today.
///
/// The bound is the host's and not the program's, for the reason ADR 0018
/// gives about `files.Reader.readLine`: a caller has no way to know how large
/// an answer it has not seen yet is, so a per-request bound would make every
/// caller answer a question about a response that has not arrived. It is
/// counted over the whole response as it arrives — status line, headers, and
/// body together, which is what the client actually holds — rather than
/// checked against a `Content-Length`, since what a peer claims never decides
/// what this host allocates.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// What `http` declares about itself.
///
/// The table is [`cove_schema::hosts::HTTP`], so the description the compiler
/// checks a call against, the one the boundary dispatches through, and the
/// one `cove trace` reads out of a recorded file are the same bytes.
const SCHEMA: ModuleSchema = cove_schema::hosts::HTTP;

/// `http`: reaching a server, and being one.
pub struct Http {
    source: HttpSource,
    /// Every listener this host still has open, by the identity it issued.
    ///
    /// This is the whole of what a handle addresses. A handle whose entry is
    /// gone — closed, or issued by some other run — finds nothing here, which
    /// is what makes a stale handle a reported error rather than a call on a
    /// socket that means something else now.
    open: Mutex<BTreeMap<u64, Listener>>,
    /// The identity the next listener gets. Zero is never issued, so a
    /// handle's number reads as the order the run opened them in.
    next_id: AtomicU64,
}

enum HttpSource {
    /// Sockets, for real.
    Real,
    /// Canned responses for `fetch`, and a scripted request queue for a
    /// listener to replay.
    Recorded {
        answers: BTreeMap<String, RecordedResponse>,
        requests: Vec<ScriptedRequest>,
        /// What every handled request answered, in order, so a test can read
        /// back what the program served.
        served: Arc<Mutex<Vec<String>>>,
    },
    /// A host with no network at all.
    Denied,
}

/// One answer a fake client gives, as a test wrote it.
///
/// It is a status and a body because that is what an `http.Response` is, so a
/// test writes down exactly what the program will see. A fake that recorded
/// only bodies could not produce the one thing a client most often has to
/// handle — a server that answered, and answered badly.
#[derive(Clone, Debug)]
pub struct RecordedResponse {
    /// The status the fake server sent, such as `200` or `404`.
    pub status: i64,
    /// The body it carried.
    pub body: String,
}

impl RecordedResponse {
    /// A `200` carrying `body`, which is what most recorded answers are.
    pub fn ok(body: &str) -> RecordedResponse {
        RecordedResponse::new(200, body)
    }

    /// A `status` carrying `body`.
    pub fn new(status: i64, body: &str) -> RecordedResponse {
        RecordedResponse {
            status,
            body: body.to_string(),
        }
    }
}

/// One request a fake listener hands to the program, as a test wrote it.
#[derive(Clone, Debug)]
pub struct ScriptedRequest {
    /// `Get` or `Post`, the case name of `http.Method`.
    pub method: String,
    /// The path, such as `/health`.
    pub path: String,
    /// The request body, which is empty for a `Get`.
    pub body: String,
}

impl ScriptedRequest {
    /// A `Get` of `path` with no body.
    pub fn get(path: &str) -> ScriptedRequest {
        ScriptedRequest {
            method: "Get".to_string(),
            path: path.to_string(),
            body: String::new(),
        }
    }

    /// A `Post` of `body` to `path`.
    pub fn post(path: &str, body: &str) -> ScriptedRequest {
        ScriptedRequest {
            method: "Post".to_string(),
            path: path.to_string(),
            body: body.to_string(),
        }
    }
}

/// One open listener, on whichever side of the boundary it really lives.
enum Listener {
    /// A bound socket.
    Real(TcpListener),
    /// A queue of requests, and the port the program asked for.
    Scripted {
        port: i64,
        requests: Vec<ScriptedRequest>,
    },
}

impl Http {
    /// A host that speaks HTTP/1.1 over TCP.
    ///
    /// `listen` binds loopback only. Granting `http` is authority to talk to
    /// the machine's own network stack, not permission to publish a service
    /// on every interface the machine has.
    pub fn real() -> Self {
        Http::with_source(HttpSource::Real)
    }

    /// A fake that answers `fetch` from `answers` and lets a listener replay
    /// `requests`, for tests.
    ///
    /// The key is the URL exactly as the program writes it. This is a
    /// recorded answer, not a client: a fake that resolved a host name would
    /// be reaching the network the grant was supposed to describe.
    pub fn recorded(
        answers: BTreeMap<String, RecordedResponse>,
        requests: Vec<ScriptedRequest>,
    ) -> Self {
        Http::with_source(HttpSource::Recorded {
            answers,
            requests,
            served: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// What this host has served so far, for a test to read back.
    ///
    /// A real host serves to a socket and keeps nothing, so it answers with
    /// an empty log: what went out is on the other end of the connection.
    pub fn served(&self) -> Served {
        match &self.source {
            HttpSource::Recorded { served, .. } => Served(Arc::clone(served)),
            _ => Served(Arc::new(Mutex::new(Vec::new()))),
        }
    }

    /// A host with no network, which refuses every call and says so.
    pub fn denied() -> Self {
        Http::with_source(HttpSource::Denied)
    }

    fn with_source(source: HttpSource) -> Self {
        Http {
            source,
            open: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Opens a listener and issues the handle that names it.
    fn listen(&self, port: i64) -> Result<Value, RuntimeError> {
        if !(0..=65535).contains(&port) {
            return Ok(Value::err(Value::error(format!(
                "http: {port} is not a port number"
            ))));
        }
        let listener = match &self.source {
            HttpSource::Real => match TcpListener::bind(("127.0.0.1", port as u16)) {
                Ok(listener) => Listener::Real(listener),
                Err(e) => {
                    return Ok(Value::err(Value::error(format!(
                        "http: cannot listen on 127.0.0.1:{port}: {e}"
                    ))))
                }
            },
            HttpSource::Recorded { requests, .. } => Listener::Scripted {
                port,
                requests: requests.clone(),
            },
            HttpSource::Denied => {
                return Ok(Value::err(Value::error(
                    "http: this host has no network, so nothing can listen",
                )))
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.locked().insert(id, listener);
        Ok(Value::ok(Value::Resource(ResourceHandle::new(
            "http",
            &SCHEMA.resources[0],
            id,
        ))))
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Listener>> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `GET url`, answering the response the server sent.
    ///
    /// Whatever status came back is an `Ok`, because a server that answered
    /// `404` answered: the status is a fact about the endpoint and the client
    /// is the wrong place to decide which facts are failures. An `Err` is
    /// reserved for a run that learned nothing — a URL this host will not
    /// send, a connection it could not make, a read that ran out of time, or
    /// a response larger than it will hold — so those two are told apart by
    /// their shape rather than by the wording of a message.
    ///
    /// `time_left` is what the run has before its deadline, which clamps how
    /// long the real client will wait for an answer; `None` leaves it bounded
    /// by [`READ_TIMEOUT`] alone.
    fn fetch(&self, url: &str, time_left: Option<Duration>) -> Value {
        match &self.source {
            HttpSource::Real => match fetch_over_tcp(url, time_left) {
                Ok((status, body)) => Value::ok(response(status, &body)),
                Err(message) => Value::err(Value::error(message)),
            },
            HttpSource::Recorded { answers, .. } => match answers.get(url) {
                Some(answer) => Value::ok(response(answer.status, &answer.body)),
                None => Value::err(Value::error(format!(
                    "http: no recorded answer for `{url}`"
                ))),
            },
            HttpSource::Denied => Value::err(Value::error(
                "http: this host has no network, so no request can be sent",
            )),
        }
    }

    /// Serves one request, and answers whether one arrived.
    ///
    /// `false` means there is nothing more to serve, which is what ends the
    /// loop the program wrote around this call. A fake listener says it when
    /// its scripted queue is empty. A real one says it when the run was
    /// cancelled, or ran out of time, while it was waiting for a connection:
    /// the wait is a poll rather than a blocking `accept`, so the run's
    /// controls reach it.
    ///
    /// Answering `false` rather than an error of its own is deliberate. The
    /// program's loop is what should end, and the reason the run stopped
    /// belongs to the budget that holds the limit — the next safepoint raises
    /// `Cancelled` or `Deadline` naming the value that was configured. A host
    /// that invented a failure here would put a second, worse account of the
    /// same event in front of the reader, and would hand a program a Cove
    /// `Err` it could catch and ignore.
    fn serve_one(
        &self,
        handle: &ResourceHandle,
        routes: &Value,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        let Value::Array(routes) = routes else {
            return Err(RuntimeError::new(format!(
                "`http.Server.handle` takes an `Array<http.Route>`, but found `{}`",
                routes.type_name()
            )));
        };
        // Whatever the next request needs is taken while the lock is held,
        // and the lock is released before the handler runs: a handler is
        // Cove code, and Cove code may call this host again.
        let next = {
            let mut open = self.locked();
            match open.get_mut(&handle.id) {
                None => return Err(stale(handle, "handle")),
                Some(Listener::Scripted { requests, .. }) => {
                    if requests.is_empty() {
                        return Ok(Value::ok(Value::Bool(false)));
                    }
                    Next::Scripted(requests.remove(0))
                }
                Some(Listener::Real(listener)) => match listener.try_clone() {
                    Ok(listener) => Next::Real(listener),
                    Err(e) => {
                        return Ok(Value::err(Value::error(format!(
                            "http: cannot accept on {handle}: {e}"
                        ))))
                    }
                },
            }
        };

        let (asked, connection) = match next {
            Next::Scripted(scripted) => (scripted, None),
            Next::Real(listener) => match accept_when_ready(&listener, back) {
                Waited::Connected(stream) => {
                    // One deadline for the whole request, and never more of it
                    // than the run itself has left.
                    let until = Instant::now() + bounded(READ_TIMEOUT, back.time_left());
                    match read_request(&stream, until) {
                        Ok((method, path, body)) => (
                            ScriptedRequest {
                                method: method_case(&method),
                                path,
                                body,
                            },
                            Some(stream),
                        ),
                        // A request that could not be read is still a request
                        // that arrived, so the peer is told why and the
                        // program's loop goes round again.
                        Err(unread) => {
                            let _ = write_response(
                                &stream,
                                unread.status,
                                &json_string(&unread.message),
                            );
                            return Ok(Value::ok(Value::Bool(true)));
                        }
                    }
                }
                Waited::Stopped => return Ok(Value::ok(Value::Bool(false))),
                Waited::Failed(e) => {
                    return Ok(Value::err(Value::error(format!(
                        "http: cannot accept on {handle}: {e}"
                    ))))
                }
            },
        };

        let (status, body) = match route_for(routes, &asked) {
            Some(handler) => {
                let answered = back.call(
                    &handler,
                    vec![request(&asked.method, &asked.path, &asked.body)],
                )?;
                response_of(&answered)?
            }
            None => (
                404,
                json_string(&format!("no route for {} {}", asked.method, asked.path)),
            ),
        };

        match connection {
            Some(stream) => {
                if let Err(message) = write_response(&stream, status, &body) {
                    return Ok(Value::err(Value::error(message)));
                }
            }
            None => self.record_served(status, &body),
        }
        Ok(Value::ok(Value::Bool(true)))
    }

    /// Remembers what a fake listener answered, so a test can read it back.
    fn record_served(&self, status: i64, body: &str) {
        if let HttpSource::Recorded { served, .. } = &self.source {
            served
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(format!("{status} {body}"));
        }
    }
}

/// Where the next request is coming from, decided while the lock is held.
enum Next {
    Scripted(ScriptedRequest),
    Real(TcpListener),
}

/// What waiting for a connection came to.
enum Waited {
    /// A client connected, and its stream is ready to be read.
    Connected(TcpStream),
    /// The run was cancelled or ran out of time while nothing was arriving.
    Stopped,
    /// The listener itself failed, which is the host's problem rather than
    /// the run's.
    Failed(std::io::Error),
}

/// Waits for one connection, in steps short enough for the run's controls to
/// reach the wait.
///
/// A blocking `accept` is the whole of the problem this exists to solve. The
/// runtime checks cancellation and the deadline before it dispatches a host
/// call and cannot check either again until the call returns, so a `handle`
/// sitting in `accept` with nobody connecting is unreachable by both: the
/// comment that used to sit on [`Http::serve_one`], promising that such a
/// program "runs until the run itself is stopped", described something the
/// implementation could not do. Polling a nonblocking listener turns the wait
/// into a loop with a place to look in it, and the looking is what makes the
/// promise true.
///
/// The listener is a clone of the one the host owns and no lock is held here,
/// which matters more than it looks: this wait is the longest thing this
/// module ever does, and holding the host's mutex across it would queue every
/// other task behind an idle server.
fn accept_when_ready(listener: &TcpListener, back: &dyn Reentry) -> Waited {
    if let Err(e) = listener.set_nonblocking(true) {
        return Waited::Failed(e);
    }
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // A nonblocking listener hands back a nonblocking stream on
                // some platforms, and reading one of those in a loop is
                // exactly the spin this function exists to avoid. The read
                // path wants an ordinary blocking socket with a timeout on
                // it, so the mode goes back before anything reads.
                return match stream.set_nonblocking(false) {
                    Ok(()) => Waited::Connected(stream),
                    Err(e) => Waited::Failed(e),
                };
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // A connection that is already waiting is served even by a
                // run that is stopping, because it cost nothing to take and
                // refusing it would leave a client with no answer at all.
                // This is where there is nothing to lose by giving up.
                if back.is_cancelled() || back.time_left().is_some_and(|left| left.is_zero()) {
                    return Waited::Stopped;
                }
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            // A signal delivered to this thread interrupts the call without
            // saying anything about the socket, so the socket is asked again.
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Waited::Failed(e),
        }
    }
}

/// The shorter of what this host allows itself and what the run has left.
///
/// A host willing to wait thirty seconds for a peer should not wait thirty
/// seconds on behalf of a run that had two hundred milliseconds to live. A
/// run with no deadline is bounded by the host's own allowance alone, which
/// is the only bound there is to apply.
fn bounded(allowance: Duration, time_left: Option<Duration>) -> Duration {
    match time_left {
        Some(left) => allowance.min(left),
        None => allowance,
    }
}

/// The handler of the first route that matches `asked`.
fn route_for(routes: &[Value], asked: &ScriptedRequest) -> Option<Value> {
    routes.iter().find_map(|route| {
        let Value::Struct(route) = route else {
            return None;
        };
        let method = match route.get("method") {
            Some(Value::Enum(method)) => method.case.to_string(),
            _ => return None,
        };
        let path = match route.get("path") {
            Some(Value::Str(path)) => path.to_string(),
            _ => return None,
        };
        (method == asked.method && path == asked.path).then(|| route.get("handler").cloned())?
    })
}

/// The status and body a handler answered with.
///
/// A handler may answer with a response, or with a `Result` carrying one:
/// both are what Cove source writes, and an `Err` is an ordinary failure the
/// server reports as a `500` rather than a reason to stop serving.
fn response_of(value: &Value) -> Result<(i64, String), RuntimeError> {
    match value {
        Value::Struct(structure) if &*structure.type_name == "http.Response" => {
            let status = match structure.get("status") {
                Some(Value::Int(status)) => *status,
                _ => 200,
            };
            let body = match structure.get("body") {
                Some(Value::Str(body)) => body.to_string(),
                Some(other) => json_of(other),
                None => String::new(),
            };
            Ok((status, body))
        }
        Value::Enum(result) if &*result.type_name == RESULT.name => match value.ok_payload() {
            Some(payload) => response_of(payload.first().unwrap_or(&Value::Unit)),
            None => Ok((
                500,
                json_string(
                    &result
                        .payload
                        .first()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                ),
            )),
        },
        other => Err(RuntimeError::new(format!(
            "a route handler must answer with an `http.Response`, but this one answered `{}`",
            other.type_name()
        ))
        .with_help("build one with `http.json(status, value)`")),
    }
}

/// The `http.Method` case a wire method name is.
fn method_case(method: &str) -> String {
    match method.to_ascii_uppercase().as_str() {
        "POST" => "Post".to_string(),
        _ => "Get".to_string(),
    }
}

/// What a fake host served, for a test to read back.
///
/// A test drives the program and then asks this what went out, rather than
/// asking the host: the host is the program's boundary, and a test that
/// reached into it would be testing the boundary rather than the program.
#[derive(Clone)]
pub struct Served(Arc<Mutex<Vec<String>>>);

impl Served {
    /// Every response the program served, in order, as `<status> <body>`.
    pub fn responses(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// A call on a handle whose resource this host no longer has.
fn stale(handle: &ResourceHandle, op: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "`{handle}` is closed, so `{op}` has nothing to act on"
    ))
    .with_rule(
        "A host resource handle names a resource the host owns. Closing the resource ends the handle; the name outlives it and addresses nothing.",
    )
    .with_help("open a new one, or move the `close` after the last use")
}

impl HostApi for Http {
    fn module_schema(&self) -> ModuleSchema {
        SCHEMA
    }

    /// `fetch` is the one module-level operation that waits, so it is the one
    /// that needs the way back: not to run a callback, but to ask how long
    /// the run it belongs to still has. Everything else here touches only
    /// what it was given.
    fn call_with(
        &self,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        match op {
            "fetch" => {
                let [Value::Str(url)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(self.fetch(url, back.time_left()))
            }
            _ => self.call(op, args),
        }
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "fetch" => {
                let [Value::Str(url)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                // Reached only by a caller holding the host directly, which
                // has no run behind it and so no deadline to be clamped by.
                Ok(self.fetch(url, None))
            }
            "json" => {
                let [Value::Int(status), body] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(response(*status, &json_of(body)))
            }
            "listen" => {
                let [Value::Int(port)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                self.listen(*port)
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }

    fn call_resource(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        match op {
            "port" => match self.locked().get(&handle.id) {
                Some(Listener::Real(listener)) => Ok(Value::Int(
                    listener
                        .local_addr()
                        .map(|a| i64::from(a.port()))
                        .unwrap_or(0),
                )),
                Some(Listener::Scripted { port, .. }) => Ok(Value::Int(*port)),
                None => Err(stale(handle, "port")),
            },
            "handle" => {
                let [routes] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                self.serve_one(handle, routes, back)
            }
            "close" => match self.locked().remove(&handle.id) {
                Some(_) => Ok(Value::ok(Value::Unit)),
                None => Err(stale(handle, "close")),
            },
            _ => unreachable!("checked by HostRegistry::call_resource"),
        }
    }
}

/// `http.Response(status: ..., body: ...)`.
fn response(status: i64, body: &str) -> Value {
    Value::Struct(Rc::new(StructValue {
        type_name: "http.Response".into(),
        fields: vec![
            ("status".into(), Value::Int(status)),
            ("body".into(), Value::Str(body.into())),
        ],
        opaque: false,
    }))
}

/// `http.Request(method: ..., path: ..., body: ...)`.
fn request(method: &str, path: &str, body: &str) -> Value {
    Value::Struct(Rc::new(StructValue {
        type_name: "http.Request".into(),
        fields: vec![
            ("method".into(), method_value(method)),
            ("path".into(), Value::Str(path.into())),
            ("body".into(), Value::Str(body.into())),
        ],
        opaque: false,
    }))
}

/// `http.Method.Get` and `http.Method.Post`.
fn method_value(case: &str) -> Value {
    Value::Enum(Box::new(EnumValue {
        type_name: "http.Method".into(),
        case: case.into(),
        payload: Vec::new(),
    }))
}

/// Renders a Cove value as JSON.
///
/// This is the encoding `http.json` names, so it is the host's and not the
/// trace's: a response body is what a client will read, with no room for the
/// tags a trace needs in order to be read back as a value.
fn json_of(value: &Value) -> String {
    match value {
        Value::Unit => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) if x.is_finite() => format!("{x:?}"),
        Value::Str(s) => json_string(s),
        Value::Array(items) => {
            let items = items.iter().map(json_of).collect::<Vec<_>>().join(",");
            format!("[{items}]")
        }
        Value::Vector(storage) => {
            let items = storage
                .elements
                .borrow()
                .iter()
                .map(json_of)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{items}]")
        }
        Value::Map(entries) => {
            let entries = entries
                .iter()
                .map(|(key, value)| format!("{}:{}", json_string(&key.to_string()), json_of(value)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{entries}}}")
        }
        Value::Struct(structure) => {
            let fields = structure
                .fields
                .iter()
                .map(|(name, field)| format!("{}:{}", json_string(name), json_of(field)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{fields}}}")
        }
        // A case with no payload is its name, which is how an enum reads as
        // JSON; one with a payload carries it alongside.
        Value::Enum(enumeration) if enumeration.payload.is_empty() => {
            json_string(&enumeration.case)
        }
        Value::Enum(enumeration) => {
            let payload = enumeration
                .payload
                .iter()
                .map(json_of)
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{}:[{payload}]}}", json_string(&enumeration.case))
        }
        // Anything else has no JSON of its own, so it goes out as what it
        // printed rather than as a shape a reader would misread.
        other => json_string(&other.to_string()),
    }
}

/// One JSON string literal, with the escapes JSON requires.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Sends one `GET` and reads the status and body that came back.
///
/// Every status the peer sent is answered, including the ones a program will
/// call failures. Deciding that here is what the old signature did, and it
/// cost the caller the difference between a server that refused and a server
/// that was never reached: both arrived as `Err` and only the wording told
/// them apart. A status is data, so it travels as data.
///
/// The read is bounded by [`READ_TIMEOUT`], clamped by whatever the run has
/// left, so a `fetch` cannot outlive its run's deadline by thirty seconds
/// waiting on a server that has stopped answering. It is bounded in size by
/// [`MAX_RESPONSE_BYTES`] as well, checked as the bytes arrive: a peer that
/// keeps sending is stopped at the bound rather than after it, which is the
/// only order in which a bound on an allocation means anything.
///
/// The connect is the one step this does not bound, and a reader should know
/// it: `TcpStream::connect` resolves a name and completes a handshake with no
/// timeout of its own, so a run whose deadline expires during either waits
/// for the platform rather than for the budget. Loopback, which is what this
/// host is granted authority over, makes both immediate.
fn fetch_over_tcp(url: &str, time_left: Option<Duration>) -> Result<(i64, String), String> {
    let (authority, path) = split_url(url)?;
    let allowance = bounded(READ_TIMEOUT, time_left);
    if allowance.is_zero() {
        return Err(format!(
            "http: the run ran out of time before {authority} could be asked"
        ));
    }
    let mut stream = TcpStream::connect(&authority)
        .map_err(|e| format!("http: cannot connect to {authority}: {e}"))?;
    stream
        .set_read_timeout(Some(allowance))
        .map_err(|e| format!("http: {e}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("http: cannot send to {authority}: {e}"))?;
    let answer = read_response_within(&stream, &authority)?;
    let answer = String::from_utf8_lossy(&answer).into_owned();
    let (head, body) = answer
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("http: {authority} sent no complete response"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<i64>().ok())
        .ok_or_else(|| format!("http: {authority} sent no status line"))?;
    Ok((status, body.to_string()))
}

/// Reads a whole response, or stops at [`MAX_RESPONSE_BYTES`] and says so.
///
/// The bound is checked after each read rather than by asking the peer how
/// much it intends to send, so a response with no `Content-Length`, a
/// dishonest one, or none at all is held to the same number. The buffer grows
/// with what arrived and never with what was claimed.
fn read_response_within(stream: &TcpStream, authority: &str) -> Result<Vec<u8>, String> {
    let mut reader = stream;
    let mut answer = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|e| format!("http: cannot read from {authority}: {e}"))?;
        if read == 0 {
            return Ok(answer);
        }
        answer.extend_from_slice(&chunk[..read]);
        if answer.len() > MAX_RESPONSE_BYTES {
            return Err(format!(
                "http: {authority} sent more than the {MAX_RESPONSE_BYTES} bytes this host reads"
            ));
        }
    }
}

/// Splits `http://host:port/path` into what to connect to and what to ask
/// for.
///
/// Only `http` is understood. A `https` URL is refused rather than fetched
/// over a plaintext socket, because silently downgrading a URL a program
/// wrote as encrypted would be the worst possible answer.
fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = match url.split_once("://") {
        Some(("http", rest)) => rest,
        Some(("https", _)) => {
            return Err(format!(
                "http: `{url}` is https, which this host does not speak"
            ))
        }
        Some((scheme, _)) => {
            return Err(format!("http: `{url}` uses the unknown scheme `{scheme}`"))
        }
        None => return Err(format!("http: `{url}` is not an absolute URL")),
    };
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("http: `{url}` names no host"));
    }
    let authority = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    Ok((authority, path.to_string()))
}

/// The reason phrase for a status, for the response line.
fn reason(status: i64) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Status",
    }
}

/// One request that could not be read, and what the peer is told about it.
struct Unread {
    /// What this becomes on the wire: `400` for a request this host could not
    /// make sense of, `408` for one whose time ran out, `413`, `414`, or
    /// `431` for one bigger than a bound, and `501` for one that asks for
    /// something this host does not do.
    status: i64,
    /// What went wrong, which is also the response body.
    message: String,
}

impl Unread {
    /// A request this host could not read at all.
    fn malformed(message: String) -> Unread {
        Unread {
            status: 400,
            message,
        }
    }

    /// A request that passed one of the bounds this host holds itself to.
    ///
    /// The status is what says which bound, and saying so is the whole reason
    /// these are not all `400`: a client told `413` knows to send less body,
    /// and a client told `431` knows to send fewer headers, where a client
    /// told "bad request" knows only that this host was unhappy.
    fn too_large(status: i64, message: String) -> Unread {
        Unread { status, message }
    }

    /// A request that is well formed and asks for something this host does
    /// not do, which is a different admission from refusing it.
    fn unsupported(message: String) -> Unread {
        Unread {
            status: 501,
            message,
        }
    }

    /// A request whose deadline passed before it was whole.
    ///
    /// A peer that stopped halfway through a request and one that is being
    /// slow on purpose look identical from here, and neither needs a
    /// different answer: the time allowed for this request is up.
    fn timed_out() -> Unread {
        Unread {
            status: 408,
            message: "http: this request did not arrive within the time allowed for it".to_string(),
        }
    }

    /// What one failed read means. A timeout is the request's own deadline
    /// running out, since that is the only timeout the socket was ever given;
    /// anything else is a connection this host cannot read.
    fn from_read(what: &str, e: std::io::Error) -> Unread {
        match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => Unread::timed_out(),
            _ => Unread::malformed(format!("http: cannot read {what}: {e}")),
        }
    }
}

/// Gives `stream` whatever is left of the deadline the whole request shares.
///
/// `set_read_timeout(Some(Duration::ZERO))` is refused by the platform, and
/// rightly: a zero timeout is how the socket API spells "no timeout", which
/// is the opposite of what an exhausted allowance is asking for. So a
/// deadline that has already passed stops here rather than being handed to
/// the socket as permission to wait forever.
fn allow_until(stream: &TcpStream, until: Instant) -> Result<(), Unread> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Unread::timed_out());
    }
    stream
        .set_read_timeout(Some(left))
        .map_err(|e| Unread::malformed(format!("http: cannot bound the read: {e}")))
}

/// Reads one HTTP/1.1 request from `stream`, giving the request line, the
/// headers, and the body together until `until` and no longer.
///
/// One deadline, not a timeout per read. A timeout that started again on
/// every successful read would bound each read and the request not at all: a
/// peer sending one byte every twenty-nine seconds keeps the call alive for
/// as long as it likes, and a run that meant to stop in two hundred
/// milliseconds waits for all of it. So the socket is re-armed before each
/// read with what remains of the one allowance, and the first read that finds
/// nothing left gives up.
fn read_request(stream: &TcpStream, until: Instant) -> Result<(String, String, String), Unread> {
    let mut reader = BufReader::new(stream);
    let line = match line_within(&mut reader, stream, until, MAX_REQUEST_LINE, "the request line")? {
        Line::Read(line) => line,
        // Nothing at all arrived, which is a connection that opened and
        // closed rather than a request to complain about.
        Line::Ended => String::new(),
        Line::TooLong => {
            return Err(Unread::too_large(
                414,
                format!(
                    "http: this request line is longer than the {MAX_REQUEST_LINE} bytes this host reads"
                ),
            ))
        }
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut headers = 0usize;
    let mut header_bytes = 0usize;
    // The text of `Content-Length` as it was sent, kept rather than parsed so
    // that a second one can be compared with it before either is trusted.
    let mut claimed: Option<String> = None;
    let mut coding: Option<String> = None;
    loop {
        let header = match line_within(&mut reader, stream, until, MAX_HEADER_BYTES, "a header")? {
            Line::Read(header) => header,
            Line::Ended => break,
            Line::TooLong => {
                return Err(Unread::too_large(
                    431,
                    format!(
                        "http: this request has a header longer than the {MAX_HEADER_BYTES} bytes this host reads"
                    ),
                ))
            }
        };
        if header.trim().is_empty() {
            break;
        }
        headers += 1;
        header_bytes += header.len();
        if headers > MAX_HEADER_COUNT {
            return Err(Unread::too_large(
                431,
                format!("http: this request has more than the {MAX_HEADER_COUNT} headers this host reads"),
            ));
        }
        if header_bytes > MAX_HEADERS_BYTES {
            return Err(Unread::too_large(
                431,
                format!(
                    "http: this request's headers are longer than the {MAX_HEADERS_BYTES} bytes this host reads"
                ),
            ));
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("content-length") {
            // RFC 9110 lets a repeated `Content-Length` stand only when every
            // one of them says the same thing. Two that disagree are two
            // requests as far as anything downstream is concerned, and
            // picking one of them is how a proxy and a server come to
            // disagree about where a body ended.
            match &claimed {
                Some(first) if first != value => {
                    return Err(Unread::malformed(format!(
                        "http: this request gives `Content-Length` as both `{first}` and `{value}`"
                    )))
                }
                _ => claimed = Some(value.to_string()),
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            coding = Some(value.to_string());
        }
    }
    // A transfer coding is where the body ends, and this host knows only the
    // one that `Content-Length` describes. Reading a chunked body as if it
    // were empty would leave its chunks on the socket and answer as though
    // there had been no body, which is a worse answer than saying no.
    if let Some(coding) = coding {
        return Err(Unread::unsupported(format!(
            "http: this host does not speak `Transfer-Encoding: {coding}`, so it cannot tell where this body ends"
        )));
    }

    let length = match &claimed {
        Some(value) => content_length(value)?,
        None => 0,
    };
    let body = body_within(&mut reader, stream, until, length)?;
    let path = target.split('?').next().unwrap_or("/").to_string();
    Ok((method, path, String::from_utf8_lossy(&body).into_owned()))
}

/// How reading one bounded line ended.
enum Line {
    /// A whole line, as it was sent, with whatever ended it still on it.
    Read(String),
    /// The peer said nothing more, which ends the headers as surely as a
    /// blank line does.
    Ended,
    /// The bound was reached with no end of line in sight. What is past it
    /// was never read, which is the point: a line is refused for its length
    /// without this host first finding out how long it really was.
    TooLong,
}

/// Reads one line, no longer than `limit` bytes and no later than `until`.
///
/// The limit is a [`Read::take`] around the reader rather than a length
/// checked afterwards, so the bound governs what is read and not merely what
/// is kept. A peer that opens a connection and sends one enormous line gets
/// `limit` bytes of this host's attention and no more.
fn line_within(
    reader: &mut BufReader<&TcpStream>,
    stream: &TcpStream,
    until: Instant,
    limit: usize,
    what: &str,
) -> Result<Line, Unread> {
    allow_until(stream, until)?;
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|e| Unread::from_read(what, e))?;
    match bytes.last() {
        None => Ok(Line::Ended),
        Some(b'\n') => Ok(Line::Read(String::from_utf8_lossy(&bytes).into_owned())),
        Some(_) if bytes.len() >= limit => Ok(Line::TooLong),
        // Short of the bound and short of a newline is a peer that stopped
        // talking in the middle of a line, which is a request that will never
        // be whole rather than one that is too big.
        Some(_) => Err(Unread::malformed(format!(
            "http: this connection ended in the middle of {what}"
        ))),
    }
}

/// The body length a `Content-Length` claims, if this host will read it.
///
/// The old reading of this header was `parse().unwrap_or(0)`, which turned
/// every unreadable length into "there is no body" — so a malformed request
/// was served as though it were a whole one, with its body still sitting on
/// the socket. A length is either a number this host will read or a reason to
/// refuse the request.
fn content_length(value: &str) -> Result<usize, Unread> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Unread::malformed(format!(
            "http: `Content-Length: {value}` is not a count of bytes"
        )));
    }
    match value.parse::<usize>() {
        Ok(length) if length <= MAX_BODY_BYTES => Ok(length),
        // A length past `MAX_BODY_BYTES` and one past what a `usize` can
        // count are the same refusal. Both are digits this host will not read
        // that many of, and neither is allocated for in order to find out.
        _ => Err(Unread::too_large(
            413,
            format!(
                "http: this request claims {value} bytes of body, and this host reads at most {MAX_BODY_BYTES}"
            ),
        )),
    }
}

/// Reads the `length` bytes of body the headers claimed, until `until`.
///
/// `length` has already been checked against [`MAX_BODY_BYTES`], and the
/// bytes are gathered as they arrive rather than into a buffer sized from the
/// claim, so a peer that says a mebibyte and sends nothing costs this host
/// nothing. The deadline is re-armed each time round for the reason
/// [`read_request`] gives: one request has one allowance, and a peer that
/// dribbles its body must not renew it a chunk at a time.
fn body_within(
    reader: &mut BufReader<&TcpStream>,
    stream: &TcpStream,
    until: Instant,
    length: usize,
) -> Result<Vec<u8>, Unread> {
    let mut body = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    while body.len() < length {
        allow_until(stream, until)?;
        let want = chunk.len().min(length - body.len());
        match reader.read(&mut chunk[..want]) {
            Ok(0) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
            // A signal says nothing about the socket, so the socket is asked
            // again with what is left of the same allowance.
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(Unread::from_read("the body", e)),
        }
    }
    if body.len() < length {
        return Err(Unread::malformed(format!(
            "http: this request claims {length} bytes of body and sent {}",
            body.len()
        )));
    }
    Ok(body)
}

/// Writes one response and closes the connection.
fn write_response(mut stream: &TcpStream, status: i64, body: &str) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(body.as_bytes()))
        .and_then(|_| stream.flush())
        .map_err(|e| format!("http: cannot send the response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Cancellation;
    use crate::host::NoReentry;
    use crate::value::MapKey;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::AtomicUsize;

    fn err_message(value: Value) -> String {
        match value.err_payload() {
            Some(payload) => payload.first().map(ToString::to_string).unwrap_or_default(),
            None => panic!("expected `Err(...)`, found {value}"),
        }
    }

    fn is_ok(value: &Value) -> bool {
        value.is_ok()
    }

    /// The status and body of the `http.Response` an `Ok` carries.
    ///
    /// Both facts together, because both together are what `fetch` now
    /// answers and a helper that read one of them would let a test pin a body
    /// while saying nothing about the status it came with.
    fn ok_response(value: Value) -> (i64, String) {
        let Some(payload) = value.ok_payload() else {
            panic!("expected `Ok(...)`, found {value}");
        };
        match payload.first() {
            Some(Value::Struct(fields)) if &*fields.type_name == "http.Response" => {
                let status = match fields.get("status") {
                    Some(Value::Int(status)) => *status,
                    other => panic!("expected an `Int` status, found {other:?}"),
                };
                let body = match fields.get("body") {
                    Some(Value::Str(body)) => body.to_string(),
                    other => panic!("expected a `String` body, found {other:?}"),
                };
                (status, body)
            }
            other => panic!("expected `Ok(http.Response)`, found {other:?}"),
        }
    }

    fn bool_ok(value: Value) -> bool {
        match value.ok_payload() {
            Some(payload) => match payload.first() {
                Some(Value::Bool(b)) => *b,
                other => panic!("expected `Ok(Bool)`, found {other:?}"),
            },
            None => panic!("expected `Ok(...)`, found {value}"),
        }
    }

    /// The body a `http.json`-built `http.Response` carries, read back for a
    /// test to check the encoding rather than the module's own plumbing.
    fn response_body(value: Value) -> String {
        match value {
            Value::Struct(structure) if &*structure.type_name == "http.Response" => {
                match structure.get("body") {
                    Some(Value::Str(body)) => body.to_string(),
                    other => panic!("expected a `String` body, found {other:?}"),
                }
            }
            other => panic!("expected an `http.Response`, found {other}"),
        }
    }

    /// Opens a listener on `port` and answers the handle it issued.
    fn listen(http: &Http, port: i64) -> Arc<ResourceHandle> {
        let answered = http.call("listen", vec![Value::Int(port)]).unwrap();
        match answered.ok_payload() {
            Some(payload) => match payload.first() {
                Some(Value::Resource(handle)) => handle.clone(),
                other => panic!("expected `Ok(Resource)`, found {other:?}"),
            },
            None => panic!("expected `Ok(...)`, found {answered}"),
        }
    }

    /// One `http.Route`, as Cove source would build it: a method, a path,
    /// and a handler the host never looks inside.
    fn route(method: &str, path: &str) -> Value {
        Value::Struct(Rc::new(StructValue {
            type_name: "http.Route".into(),
            fields: vec![
                (
                    "method".into(),
                    Value::Enum(Box::new(EnumValue {
                        type_name: "http.Method".into(),
                        case: method.into(),
                        payload: Vec::new(),
                    })),
                ),
                ("path".into(), Value::Str(path.into())),
                ("handler".into(), Value::Unit),
            ],
            opaque: false,
        }))
    }

    /// Reads and discards one HTTP/1.1 request's headers, so a test server
    /// can answer only after the client has actually sent its request —
    /// closing a socket with unread bytes still sitting in it can reset the
    /// connection before the response goes out.
    fn read_request_head(stream: &TcpStream) {
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .expect("reading a header line should succeed");
            if read == 0 || line == "\r\n" || line == "\n" {
                break;
            }
        }
    }

    /// A stub [`Reentry`] for tests, standing in for the interpreter: it runs
    /// the boxed closure it was built with instead of dispatching a route's
    /// handler into Cove code, and answers for a run that a test controls —
    /// one it can cancel, and one it can give a deadline to.
    struct StubReentry {
        calls: usize,
        respond: Box<dyn FnMut() -> Result<Value, RuntimeError>>,
        /// Every request the host handed a handler, so a test can ask what
        /// arrived rather than only what went back.
        seen: Rc<RefCell<Vec<Value>>>,
        /// The flag standing in for everything a safepoint would stop on.
        stop: Cancellation,
        /// When the run this stub stands for runs out of time, if a test gave
        /// it a deadline. An instant rather than a duration, so the answer
        /// shrinks while the host waits exactly as the real one does.
        expires_at: Option<Instant>,
        /// How many times the host asked whether the run had been stopped,
        /// which is how a test tells a poll from a spin.
        looks: Arc<AtomicUsize>,
    }

    impl StubReentry {
        fn new(respond: impl FnMut() -> Result<Value, RuntimeError> + 'static) -> Self {
            StubReentry {
                calls: 0,
                respond: Box::new(respond),
                seen: Rc::new(RefCell::new(Vec::new())),
                stop: Cancellation::new(),
                expires_at: None,
                looks: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// The flag this stub reports, for a test to raise from a thread of
        /// its own while the host is waiting.
        fn stop(&self) -> Cancellation {
            self.stop.clone()
        }

        /// Reports a run with `left` to live from now.
        fn expiring_in(mut self, left: Duration) -> Self {
            self.expires_at = Some(Instant::now() + left);
            self
        }

        /// How many times the host looked at the run's state.
        fn looks(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.looks)
        }

        /// The requests the host handed a handler, in order.
        fn seen(&self) -> Rc<RefCell<Vec<Value>>> {
            Rc::clone(&self.seen)
        }
    }

    impl Reentry for StubReentry {
        fn call(&mut self, _callee: &Value, args: Vec<Value>) -> Result<Value, RuntimeError> {
            self.calls += 1;
            self.seen.borrow_mut().extend(args);
            (self.respond)()
        }

        fn call_until(
            &mut self,
            callee: &Value,
            args: Vec<Value>,
            _stop: &Cancellation,
        ) -> Result<Value, RuntimeError> {
            self.call(callee, args)
        }

        fn is_cancelled(&self) -> bool {
            self.looks.fetch_add(1, Ordering::Relaxed);
            self.stop.is_cancelled()
        }

        fn time_left(&self) -> Option<Duration> {
            self.expires_at
                .map(|at| at.saturating_duration_since(Instant::now()))
        }

        /// A stub stands in for the entry's own way back, which is the task a
        /// call made outside any spawned task belongs to.
        fn task(&self) -> u64 {
            crate::runtime::ENTRY_TASK
        }
    }

    #[test]
    fn a_denied_host_refuses_to_fetch() {
        let http = Http::denied();
        let answer = http
            .call("fetch", vec![Value::Str("http://example.com/".into())])
            .unwrap();
        assert_eq!(
            err_message(answer),
            "http: this host has no network, so no request can be sent"
        );
    }

    #[test]
    fn a_denied_host_refuses_to_listen() {
        let http = Http::denied();
        let answer = http.call("listen", vec![Value::Int(8080)]).unwrap();
        assert_eq!(
            err_message(answer),
            "http: this host has no network, so nothing can listen"
        );
    }

    #[test]
    fn a_recorded_fetch_answers_its_response() {
        let http = Http::recorded(
            BTreeMap::from([(
                "http://example.com/".to_string(),
                RecordedResponse::ok("hello"),
            )]),
            Vec::new(),
        );
        let answer = http
            .call("fetch", vec![Value::Str("http://example.com/".into())])
            .unwrap();
        assert_eq!(ok_response(answer), (200, "hello".to_string()));
    }

    /// A fake can record a status a program will call a failure, and the
    /// program is handed it as an answer.
    ///
    /// This is what a table of bodies could not express. A test that wanted
    /// to drive a program's `404` handling had no way to say `404`, so the
    /// only failure a fake could produce was "no recorded answer", which is
    /// the shape of a connection that was never made.
    #[test]
    fn a_recorded_fetch_answers_a_status_outside_the_2xx_range() {
        let http = Http::recorded(
            BTreeMap::from([(
                "http://example.com/missing".to_string(),
                RecordedResponse::new(404, "gone"),
            )]),
            Vec::new(),
        );
        let answer = http
            .call(
                "fetch",
                vec![Value::Str("http://example.com/missing".into())],
            )
            .unwrap();
        assert_eq!(ok_response(answer), (404, "gone".to_string()));
    }

    #[test]
    fn a_fetch_the_fake_has_no_answer_for_says_so() {
        let http = Http::recorded(BTreeMap::new(), Vec::new());
        let answer = http
            .call(
                "fetch",
                vec![Value::Str("http://example.com/missing".into())],
            )
            .unwrap();
        assert_eq!(
            err_message(answer),
            "http: no recorded answer for `http://example.com/missing`"
        );
    }

    #[test]
    fn listen_issues_a_task_safe_server_handle() {
        let http = Http::recorded(BTreeMap::new(), Vec::new());
        let handle = listen(&http, 0);
        assert_eq!(handle.qualified_type(), "http.Server");
        assert!(handle.task_safe);
    }

    #[test]
    fn port_answers_the_port_the_program_asked_for() {
        let http = Http::recorded(BTreeMap::new(), Vec::new());
        let handle = listen(&http, 4242);
        match http
            .call_resource(&handle, "port", Vec::new(), &mut NoReentry)
            .unwrap()
        {
            Value::Int(port) => assert_eq!(port, 4242),
            other => panic!("expected an `Int`, found {other}"),
        }
    }

    #[test]
    fn handle_routes_a_matching_request_to_its_handler() {
        let http = Http::recorded(BTreeMap::new(), vec![ScriptedRequest::get("/health")]);
        let handle = listen(&http, 0);

        let routes = Value::Array(vec![route("Get", "/health")].into());
        let mut back = StubReentry::new(|| Ok(response(200, "healthy")));
        let answer = http
            .call_resource(&handle, "handle", vec![routes], &mut back)
            .unwrap();

        assert!(
            bool_ok(answer),
            "a scripted request should have been served"
        );
        assert_eq!(back.calls, 1, "the handler runs exactly once per request");
        assert_eq!(http.served().responses(), vec!["200 healthy".to_string()]);
    }

    #[test]
    fn handle_answers_404_for_an_unrouted_request() {
        let http = Http::recorded(BTreeMap::new(), vec![ScriptedRequest::get("/missing")]);
        let handle = listen(&http, 0);

        let routes = Value::Array(vec![route("Get", "/health")].into());
        let answer = http
            .call_resource(&handle, "handle", vec![routes], &mut NoReentry)
            .unwrap();

        assert!(
            bool_ok(answer),
            "an unrouted request is still served, just with a 404"
        );
        assert_eq!(
            http.served().responses(),
            vec!["404 \"no route for Get /missing\"".to_string()]
        );
    }

    #[test]
    fn handle_drains_its_scripted_queue_then_answers_false() {
        let http = Http::recorded(BTreeMap::new(), vec![ScriptedRequest::get("/health")]);
        let handle = listen(&http, 0);
        let routes = Value::Array(vec![route("Get", "/health")].into());
        let mut back = StubReentry::new(|| Ok(response(200, "healthy")));

        let first = http
            .call_resource(&handle, "handle", vec![routes.clone()], &mut back)
            .unwrap();
        assert!(bool_ok(first), "the one scripted request should be served");

        let second = http
            .call_resource(&handle, "handle", vec![routes], &mut back)
            .unwrap();
        assert!(
            !bool_ok(second),
            "an empty queue answers false rather than waiting for more"
        );
    }

    /// Runs `body` on a thread of its own and fails if it has not finished
    /// within `limit`.
    ///
    /// The rule below is one a host breaks by deadlocking, which is a way of
    /// failing that a test asserting on a result never reaches. This makes a
    /// regression a failure with a message rather than a suite that never
    /// ends.
    fn within<T: Send + 'static>(limit: Duration, body: impl FnOnce() -> T + Send + 'static) -> T {
        let (finished, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = finished.send(body());
        });
        done.recv_timeout(limit)
            .unwrap_or_else(|_| panic!("this did not finish within {limit:?}"))
    }

    /// A host may not hold a lock of its own while it runs a Cove callback,
    /// because the callback is Cove code and Cove code may call the same host
    /// again. `serve_one` keeps the rule by taking what the next request
    /// needs out from under the table of open listeners and dropping the
    /// guard before the handler runs.
    ///
    /// So a handler may ask the very handle it is being served by for its
    /// port, and may open a second listener, and both answer. Held across the
    /// callback, either would deadlock this task on a lock three frames up
    /// its own stack — which is why the whole thing runs under a bound.
    #[test]
    fn a_handler_may_call_back_into_the_same_host_that_is_serving_it() {
        let (served, answered) = within(Duration::from_secs(10), || {
            let http = Arc::new(Http::recorded(
                BTreeMap::new(),
                vec![ScriptedRequest::get("/health")],
            ));
            let handle = listen(&http, 4242);
            let inside = Arc::clone(&http);
            let serving = Arc::clone(&handle);
            let mut back = StubReentry::new(move || {
                let port = inside.call_resource(&serving, "port", Vec::new(), &mut NoReentry)?;
                let second = inside.call("listen", vec![Value::Int(8080)])?;
                assert!(is_ok(&second), "a second listener opened: {second}");
                Ok(response(200, &format!("{port}")))
            });

            let routes = Value::Array(vec![route("Get", "/health")].into());
            let answer = http
                .call_resource(&handle, "handle", vec![routes], &mut back)
                .unwrap();
            (http.served().responses(), bool_ok(answer))
        });
        assert!(answered, "the scripted request was served");
        assert_eq!(served, vec!["200 4242".to_string()]);
    }

    /// A handle is a name; closing the resource ends what it named, and a
    /// later call on the same name is a reported error rather than a call on
    /// whatever occupies the slot now.
    #[test]
    fn close_ends_the_handle_and_a_later_call_reports_it() {
        let http = Http::recorded(BTreeMap::new(), Vec::new());
        let handle = listen(&http, 0);

        let closed = http
            .call_resource(&handle, "close", Vec::new(), &mut NoReentry)
            .unwrap();
        assert!(is_ok(&closed), "{closed}");

        let error = http
            .call_resource(&handle, "port", Vec::new(), &mut NoReentry)
            .expect_err("a closed handle's port cannot be read");
        assert_eq!(
            error.message,
            format!("`{handle}` is closed, so `port` has nothing to act on")
        );
    }

    #[test]
    fn json_encodes_a_struct_as_an_object() {
        let http = Http::denied();
        let payload = Value::Struct(Rc::new(StructValue {
            type_name: "demo.Point".into(),
            fields: vec![("x".into(), Value::Int(1)), ("y".into(), Value::Int(2))],
            opaque: false,
        }));
        let answer = http.call("json", vec![Value::Int(200), payload]).unwrap();
        assert_eq!(response_body(answer), "{\"x\":1,\"y\":2}");
    }

    #[test]
    fn json_encodes_a_map_as_an_object() {
        let http = Http::denied();
        let mut map = BTreeMap::new();
        map.insert(MapKey::Str("a".to_string()), Value::Int(1));
        let answer = http
            .call("json", vec![Value::Int(200), Value::Map(Rc::new(map))])
            .unwrap();
        assert_eq!(response_body(answer), "{\"a\":1}");
    }

    #[test]
    fn json_encodes_a_string_with_its_quotes() {
        let http = Http::denied();
        let answer = http
            .call("json", vec![Value::Int(200), Value::Str("hi".into())])
            .unwrap();
        assert_eq!(response_body(answer), "\"hi\"");
    }

    #[test]
    fn json_encodes_an_array() {
        let http = Http::denied();
        let payload = Value::Array(vec![Value::Int(1), Value::Int(2)].into());
        let answer = http.call("json", vec![Value::Int(200), payload]).unwrap();
        assert_eq!(response_body(answer), "[1,2]");
    }

    #[test]
    fn json_encodes_a_payload_free_enum_case_as_its_name() {
        let http = Http::denied();
        let payload = Value::Enum(Box::new(EnumValue {
            type_name: "demo.Color".into(),
            case: "Red".into(),
            payload: Vec::new(),
        }));
        let answer = http.call("json", vec![Value::Int(200), payload]).unwrap();
        assert_eq!(response_body(answer), "\"Red\"");
    }

    #[test]
    fn json_escapes_a_quote_and_a_newline() {
        let http = Http::denied();
        let payload = Value::Str("a\"b\nc".into());
        let answer = http.call("json", vec![Value::Int(200), payload]).unwrap();
        assert_eq!(response_body(answer), r#""a\"b\nc""#);
    }

    #[test]
    fn split_url_refuses_https() {
        assert_eq!(
            split_url("https://example.com/").unwrap_err(),
            "http: `https://example.com/` is https, which this host does not speak"
        );
    }

    #[test]
    fn split_url_refuses_an_unknown_scheme() {
        assert_eq!(
            split_url("ftp://example.com/").unwrap_err(),
            "http: `ftp://example.com/` uses the unknown scheme `ftp`"
        );
    }

    #[test]
    fn split_url_refuses_a_non_absolute_url() {
        assert_eq!(
            split_url("example.com/path").unwrap_err(),
            "http: `example.com/path` is not an absolute URL"
        );
    }

    #[test]
    fn a_real_fetch_reads_the_body_a_2xx_response_carries() {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("binding to loopback should succeed");
        let port = listener
            .local_addr()
            .expect("the bound address should be known")
            .port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("accepting the one connection should succeed");
            read_request_head(&stream);
            let body = "hello from loopback";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("writing the canned response should succeed");
        });

        let url = format!("http://127.0.0.1:{port}/");
        let answer = Http::real()
            .call("fetch", vec![Value::Str(url.into())])
            .unwrap();
        server.join().expect("the server thread should not panic");

        assert_eq!(
            ok_response(answer),
            (200, "hello from loopback".to_string())
        );
    }

    /// The real client reads a status outside 200-299 off the wire and hands
    /// it on, with the body that came with it.
    ///
    /// This is the same server and the same client as the test above, and the
    /// only thing that differs is the status line, which is the point: a
    /// `404` is answered rather than refused, so the two tests differ by the
    /// number they assert and not by the shape they assert it in.
    #[test]
    fn a_real_fetch_answers_a_non_2xx_status_and_its_body() {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("binding to loopback should succeed");
        let port = listener
            .local_addr()
            .expect("the bound address should be known")
            .port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("accepting the one connection should succeed");
            read_request_head(&stream);
            let body = "not found here";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("writing the canned response should succeed");
        });

        let url = format!("http://127.0.0.1:{port}/");
        let answer = Http::real()
            .call("fetch", vec![Value::Str(url.clone().into())])
            .unwrap();
        server.join().expect("the server thread should not panic");

        assert_eq!(ok_response(answer), (404, "not found here".to_string()));
    }

    /// A response past [`MAX_RESPONSE_BYTES`] is an error and not an
    /// allocation.
    ///
    /// The server here promises a `Content-Length` it then makes good on, so
    /// what stops the read is the bound rather than a peer caught lying: the
    /// client counts what arrives, and a server that means to send two
    /// mebibytes is stopped at one whether or not it said so first. The
    /// message names the bound, because a client told only that something was
    /// too large learns nothing it can act on.
    #[test]
    fn a_real_fetch_refuses_a_response_past_the_bound() {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("binding to loopback should succeed");
        let port = listener
            .local_addr()
            .expect("the bound address should be known")
            .port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("accepting the one connection should succeed");
            read_request_head(&stream);
            let body = "a".repeat(MAX_RESPONSE_BYTES + 1);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            // A client that stops reading at the bound closes the connection,
            // so the rest of this write may be refused. That is the bound
            // working rather than the server failing, so the result is
            // dropped.
            let _ = stream.write_all(response.as_bytes());
        });

        let url = format!("http://127.0.0.1:{port}/");
        let answer = Http::real()
            .call("fetch", vec![Value::Str(url.into())])
            .unwrap();
        let _ = server.join();

        assert!(
            err_message(answer).ends_with(&format!(
                "sent more than the {MAX_RESPONSE_BYTES} bytes this host reads"
            )),
            "the bound is named"
        );
    }

    /// Exercises the real host both ways at once: `listen` binds an ephemeral
    /// port, a client thread reaches it as an ordinary HTTP client would, and
    /// `handle` (driven by a stub reentry standing in for the interpreter)
    /// answers the request the way a program's own route handler would.
    #[test]
    fn the_real_host_serves_a_request_end_to_end_over_loopback() {
        let http = Http::real();
        let opened = http.call("listen", vec![Value::Int(0)]).unwrap();
        let Value::Enum(result) = opened else {
            panic!("expected `Ok(...)`");
        };
        let Some(Value::Resource(handle)) = result.payload.into_iter().next() else {
            panic!("`listen` should answer a handle");
        };
        let port = match http
            .call_resource(&handle, "port", Vec::new(), &mut NoReentry)
            .unwrap()
        {
            Value::Int(port) => port,
            other => panic!("expected an `Int` port, found {other}"),
        };

        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port as u16))
                .expect("connecting to the loopback listener should succeed");
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .expect("writing the request should succeed");
            let mut answer = Vec::new();
            stream
                .read_to_end(&mut answer)
                .expect("reading the response should succeed");
            String::from_utf8_lossy(&answer).into_owned()
        });

        let routes = Value::Array(vec![route("Get", "/health")].into());
        let mut back = StubReentry::new(|| Ok(response(200, "healthy")));
        let served = bool_ok(
            http.call_resource(&handle, "handle", vec![routes], &mut back)
                .unwrap(),
        );
        assert!(served, "the listener should have answered one request");

        let received = client.join().expect("the client thread should not panic");
        assert!(received.starts_with("HTTP/1.1 200"), "{received}");
        assert!(received.ends_with("healthy"), "{received}");

        http.call_resource(&handle, "close", Vec::new(), &mut NoReentry)
            .unwrap();
    }

    /// How long a test is willing to let a `handle` that should have stopped
    /// go on waiting.
    ///
    /// Generous, because the assertion is about the difference between
    /// stopping and not stopping rather than about latency: a call that is
    /// genuinely blocked in `accept` with nobody connecting never returns at
    /// all, and one that polls returns within a couple of poll intervals even
    /// on a machine with nothing to spare.
    const PROMPTLY: Duration = Duration::from_secs(2);

    /// A real listener with nobody connecting to it, and the routing table a
    /// program would hand `handle`.
    ///
    /// Nothing is ever routed in these tests. The point of each of them is
    /// what happens before a request exists.
    fn quiet_listener() -> (Http, Arc<ResourceHandle>, Value) {
        let http = Http::real();
        let handle = listen(&http, 0);
        (
            http,
            handle,
            Value::Array(vec![route("Get", "/health")].into()),
        )
    }

    /// Cancelling a run that is waiting for a connection stops the wait.
    ///
    /// This is the acceptance test for the whole change, and it uses the real
    /// host: a real socket, bound to loopback, with nothing on the other end
    /// of it. A blocking `accept` would hang here forever and the test would
    /// have to be killed.
    #[test]
    fn cancelling_a_run_waiting_for_a_connection_stops_the_wait() {
        let (http, handle, routes) = quiet_listener();
        let mut back = StubReentry::new(|| panic!("nothing connects, so no handler runs"));
        let stop = back.stop();
        let raised_after = Duration::from_millis(50);
        // The clock is read before the thread that raises the flag can exist,
        // so the flag cannot be raised earlier than `started` plus the sleep
        // and the lower bound below is arithmetic rather than a race.
        let started = Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(raised_after);
            stop.cancel();
        });

        let answer = http
            .call_resource(&handle, "handle", vec![routes], &mut back)
            .unwrap();
        let waited = started.elapsed();

        assert!(
            !bool_ok(answer),
            "a listener that was stopped has nothing more to serve"
        );
        assert!(
            waited >= raised_after,
            "the wait ended before there was anything to end it, after {waited:?}"
        );
        assert!(
            waited < PROMPTLY,
            "the wait outlived the cancellation by {waited:?}"
        );
        assert_eq!(back.calls, 0, "no request arrived, so no handler ran");
    }

    /// The same shape, with the run's deadline running out instead of a flag
    /// being raised: a host that only watched cancellation would still sit
    /// here until a client arrived.
    #[test]
    fn a_run_deadline_that_expires_while_waiting_for_a_connection_ends_the_wait() {
        let (http, handle, routes) = quiet_listener();
        let left = Duration::from_millis(50);
        // The stub's deadline runs from the moment it is built, so the clock
        // is read first: `started` is then no later than the deadline's own
        // origin, and the lower bound below cannot be lost to a rounding.
        let started = Instant::now();
        let mut back =
            StubReentry::new(|| panic!("nothing connects, so no handler runs")).expiring_in(left);

        let answer = http
            .call_resource(&handle, "handle", vec![routes], &mut back)
            .unwrap();
        let waited = started.elapsed();

        assert!(
            !bool_ok(answer),
            "a listener whose run has run out of time has nothing more to serve"
        );
        assert!(
            waited >= left,
            "the wait ended before the deadline it was waiting for, after {waited:?}"
        );
        assert!(
            waited < PROMPTLY,
            "the wait outlived the deadline by {waited:?}"
        );
    }

    /// The bound is a poll and not a spin.
    ///
    /// There is no portable way to ask this process how much CPU it burned,
    /// so the test asks the thing the host itself does: how many times it
    /// looked at the run while it waited. A loop sleeping
    /// [`ACCEPT_POLL_INTERVAL`] looks a few hundred times a second; a loop
    /// with no sleep in it would look millions of times over the same
    /// interval, so the two are never in danger of being confused.
    #[test]
    fn waiting_for_a_connection_polls_rather_than_spinning() {
        let (http, handle, routes) = quiet_listener();
        let waiting = Duration::from_millis(200);
        let mut back = StubReentry::new(|| panic!("nothing connects, so no handler runs"))
            .expiring_in(waiting);
        let looks = back.looks();

        http.call_resource(&handle, "handle", vec![routes], &mut back)
            .unwrap();

        let looks = looks.load(Ordering::Relaxed);
        let sleeping = (waiting.as_millis() / ACCEPT_POLL_INTERVAL.as_millis()) as usize;
        assert!(looks >= 1, "the host never looked at the run at all");
        assert!(
            looks < sleeping * 20,
            "{looks} looks in {waiting:?} is a spin, not a poll at one every {ACCEPT_POLL_INTERVAL:?}"
        );
    }

    /// A peer that connects, says half of a request line, and then says
    /// nothing is answered `408` on the run's own deadline rather than on
    /// `READ_TIMEOUT`, which is thirty seconds away.
    ///
    /// This is the other half of the same problem. The socket had a timeout
    /// before this change too, but it was the host's alone, and it started
    /// again on every successful read.
    #[test]
    fn a_run_deadline_that_expires_while_reading_answers_408() {
        let http = Http::real();
        let handle = listen(&http, 0);
        let port = match http
            .call_resource(&handle, "port", Vec::new(), &mut NoReentry)
            .unwrap()
        {
            Value::Int(port) => port,
            other => panic!("expected an `Int` port, found {other}"),
        };

        // Half a request line, and then the peer holds the connection open
        // and says nothing more until it is answered.
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port as u16))
                .expect("connecting to the loopback listener should succeed");
            stream
                .write_all(b"GET /health HTT")
                .expect("writing half a request line should succeed");
            let mut answer = Vec::new();
            stream
                .read_to_end(&mut answer)
                .expect("reading the response should succeed");
            String::from_utf8_lossy(&answer).into_owned()
        });

        let routes = Value::Array(vec![route("Get", "/health")].into());
        let mut back = StubReentry::new(|| panic!("no whole request arrives, so no handler runs"))
            .expiring_in(Duration::from_millis(150));
        let started = Instant::now();
        let answer = http
            .call_resource(&handle, "handle", vec![routes], &mut back)
            .unwrap();
        let took = started.elapsed();

        assert!(
            bool_ok(answer),
            "a request that arrived and could not be read is still one that arrived"
        );
        assert!(
            took < PROMPTLY,
            "the read waited on `READ_TIMEOUT` rather than on the run, for {took:?}"
        );
        let received = client.join().expect("the client thread should not panic");
        assert!(
            received.starts_with("HTTP/1.1 408 Request Timeout"),
            "{received}"
        );

        http.call_resource(&handle, "close", Vec::new(), &mut NoReentry)
            .unwrap();
    }

    /// The clamp itself, without a socket: a run with less time left than the
    /// host's own allowance decides the allowance, and a run with no deadline
    /// leaves it alone.
    #[test]
    fn an_allowance_is_the_shorter_of_the_hosts_and_the_runs() {
        assert_eq!(bounded(READ_TIMEOUT, None), READ_TIMEOUT);
        assert_eq!(
            bounded(READ_TIMEOUT, Some(Duration::from_millis(200))),
            Duration::from_millis(200)
        );
        assert_eq!(
            bounded(READ_TIMEOUT, Some(Duration::from_secs(600))),
            READ_TIMEOUT
        );
        assert_eq!(bounded(READ_TIMEOUT, Some(Duration::ZERO)), Duration::ZERO);
    }

    /// What one raw request at a real listener came to.
    struct Exchange {
        /// What `handle` answered: whether a request arrived at all.
        served: bool,
        /// What the peer read back, status line first.
        received: String,
        /// How long `handle` took, so a refusal that had to read the whole
        /// oversized thing first can be told from one that did not.
        took: Duration,
        /// The requests that reached a handler, which for a refused one is
        /// none.
        seen: Vec<Value>,
    }

    /// Sends `request` to a real listener byte for byte and reports what came
    /// of it.
    ///
    /// The bytes go out exactly as written, which is the point: these tests
    /// are about requests no client library would let anyone send. The write
    /// is allowed to fail, because a host that refuses a request it has not
    /// finished reading closes a socket with bytes still in it, and the peer
    /// learns about that as an error on whichever call is in flight.
    fn serve_raw(request: Vec<u8>) -> Exchange {
        let http = Http::real();
        let handle = listen(&http, 0);
        let port = match http
            .call_resource(&handle, "port", Vec::new(), &mut NoReentry)
            .unwrap()
        {
            Value::Int(port) => port,
            other => panic!("expected an `Int` port, found {other}"),
        };

        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port as u16))
                .expect("connecting to the loopback listener should succeed");
            // A peer that is never answered must not hang this test; the
            // answer, or the lack of one, is what is asserted on.
            stream
                .set_read_timeout(Some(PROMPTLY))
                .expect("bounding the client's own read should succeed");
            let _ = stream.write_all(&request);
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer);
            String::from_utf8_lossy(&answer).into_owned()
        });

        let routes = Value::Array(vec![route("Get", "/health"), route("Post", "/echo")].into());
        let mut back = StubReentry::new(|| Ok(response(200, "healthy")));
        let seen = back.seen();
        let started = Instant::now();
        let served = bool_ok(
            http.call_resource(&handle, "handle", vec![routes], &mut back)
                .unwrap(),
        );
        let took = started.elapsed();
        let received = client.join().expect("the client thread should not panic");
        http.call_resource(&handle, "close", Vec::new(), &mut NoReentry)
            .unwrap();
        let seen = seen.borrow().clone();
        Exchange {
            served,
            received,
            took,
            seen,
        }
    }

    /// The status line of what the peer read back.
    fn status_line(exchange: &Exchange) -> &str {
        exchange
            .received
            .lines()
            .next()
            .unwrap_or_else(|| panic!("the peer was told nothing at all"))
    }

    /// Asserts that a request was refused with `status`, told nobody about
    /// it, and did not have to be read to its end first.
    fn refused(exchange: &Exchange, status: &str) {
        assert_eq!(
            status_line(exchange),
            status,
            "the peer was told: {}",
            exchange.received
        );
        assert!(
            exchange.served,
            "a request that arrived and was refused is still one that arrived"
        );
        assert!(
            exchange.seen.is_empty(),
            "a refused request must not reach a handler"
        );
        assert!(
            exchange.took < PROMPTLY,
            "the refusal took {:?}, which is long enough to have read the whole thing",
            exchange.took
        );
    }

    #[test]
    fn a_request_line_past_the_bound_is_refused_with_414() {
        let target = "/".to_string() + &"a".repeat(MAX_REQUEST_LINE);
        let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        refused(
            &serve_raw(request.into_bytes()),
            "HTTP/1.1 414 URI Too Long",
        );
    }

    #[test]
    fn a_header_past_the_bound_is_refused_with_431() {
        let request = format!(
            "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Long: {}\r\n\r\n",
            "a".repeat(MAX_HEADER_BYTES)
        );
        refused(
            &serve_raw(request.into_bytes()),
            "HTTP/1.1 431 Request Header Fields Too Large",
        );
    }

    #[test]
    fn more_headers_than_the_bound_are_refused_with_431() {
        let mut request = "GET /health HTTP/1.1\r\n".to_string();
        // Short enough that no number of them reaches the byte total first,
        // so this test is about the count and nothing else.
        for n in 0..=MAX_HEADER_COUNT {
            request.push_str(&format!("X-{n}: 1\r\n"));
        }
        request.push_str("\r\n");
        assert!(
            request.len() < MAX_HEADERS_BYTES,
            "this request should pass the count bound, not the byte one"
        );
        refused(
            &serve_raw(request.into_bytes()),
            "HTTP/1.1 431 Request Header Fields Too Large",
        );
    }

    #[test]
    fn more_header_bytes_than_the_bound_are_refused_with_431() {
        let mut request = "GET /health HTTP/1.1\r\n".to_string();
        // Each one is well inside the single-header bound, and there are
        // fewer than the count allows, so only their total can refuse this.
        let padding = "a".repeat(4 * 1024);
        for n in 0..16 {
            request.push_str(&format!("X-{n}: {padding}\r\n"));
        }
        request.push_str("\r\n");
        refused(
            &serve_raw(request.into_bytes()),
            "HTTP/1.1 431 Request Header Fields Too Large",
        );
    }

    #[test]
    fn a_body_past_the_bound_is_refused_with_413() {
        let request = format!(
            "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        refused(
            &serve_raw(request.into_bytes()),
            "HTTP/1.1 413 Payload Too Large",
        );
    }

    /// A claim nobody could honour is refused on the claim.
    ///
    /// The peer sends no body at all — only a header saying it is about to
    /// send nine hundred and ninety-nine terabytes of one. A host that sized
    /// a buffer from the claim would fail here in a way this process would
    /// not survive; a host that checked the claim first answers `413` in the
    /// time it takes to read one header, which is what the timing asserts.
    #[test]
    fn a_preposterous_content_length_is_refused_before_any_of_it_is_read() {
        let request =
            "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 999999999999999\r\n\r\n";
        refused(
            &serve_raw(request.as_bytes().to_vec()),
            "HTTP/1.1 413 Payload Too Large",
        );
    }

    #[test]
    fn a_content_length_that_is_not_a_number_is_refused_with_400() {
        let request = "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: some\r\n\r\n";
        refused(
            &serve_raw(request.as_bytes().to_vec()),
            "HTTP/1.1 400 Bad Request",
        );
    }

    #[test]
    fn two_content_lengths_that_disagree_are_refused_with_400() {
        let request =
            "POST /echo HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabc\r\n\r\n";
        refused(
            &serve_raw(request.as_bytes().to_vec()),
            "HTTP/1.1 400 Bad Request",
        );
    }

    /// RFC 9110 lets a repeated `Content-Length` stand when they agree, so
    /// this one is served rather than refused.
    #[test]
    fn two_content_lengths_that_agree_are_served() {
        let request = "POST /echo HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\nabc";
        let exchange = serve_raw(request.as_bytes().to_vec());
        assert_eq!(status_line(&exchange), "HTTP/1.1 200 OK");
        assert_eq!(body_of(&exchange.seen), "abc");
    }

    #[test]
    fn a_transfer_encoding_is_refused_with_501() {
        let request =
            "POST /echo HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n";
        refused(
            &serve_raw(request.as_bytes().to_vec()),
            "HTTP/1.1 501 Not Implemented",
        );
    }

    /// The bounds are for requests that pass them, and this one does not.
    #[test]
    fn a_request_with_an_ordinary_body_is_still_served() {
        let body = "{\"name\":\"cove\"}";
        let request = format!(
            "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let exchange = serve_raw(request.into_bytes());
        assert_eq!(status_line(&exchange), "HTTP/1.1 200 OK");
        assert!(
            exchange.received.ends_with("healthy"),
            "{}",
            exchange.received
        );
        assert_eq!(body_of(&exchange.seen), body);
    }

    /// A body of exactly [`MAX_BODY_BYTES`] is inside the bound, not past it.
    #[test]
    fn a_body_of_exactly_the_bound_is_served() {
        let body = "a".repeat(MAX_BODY_BYTES);
        let request = format!(
            "POST /echo HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let exchange = serve_raw(request.into_bytes());
        assert_eq!(status_line(&exchange), "HTTP/1.1 200 OK");
        assert_eq!(body_of(&exchange.seen).len(), MAX_BODY_BYTES);
    }

    /// The body of the one request a handler was given.
    fn body_of(seen: &[Value]) -> String {
        match seen {
            [Value::Struct(request)] => match request.get("body") {
                Some(Value::Str(body)) => body.to_string(),
                other => panic!("expected a `String` body, found {other:?}"),
            },
            other => panic!("expected exactly one request, found {other:?}"),
        }
    }

    /// The reading of `Content-Length` on its own, where a value too big for
    /// a `usize` can be written down without a socket having to carry it.
    #[test]
    fn a_content_length_is_a_count_of_bytes_or_a_refusal() {
        assert_eq!(content_length("0").ok(), Some(0));
        assert_eq!(content_length("12").ok(), Some(12));
        assert_eq!(
            content_length(&MAX_BODY_BYTES.to_string()).ok(),
            Some(MAX_BODY_BYTES)
        );

        for value in ["", "-1", "1 2", "0x10", "12kb", "+3", "one"] {
            let refused = content_length(value).expect_err("this is not a count");
            assert_eq!(refused.status, 400, "`{value}` is malformed, not too big");
        }

        // Past the bound, past a `usize`, and past anything at all: one
        // refusal, and none of them reserve what they claim.
        for value in [
            (MAX_BODY_BYTES + 1).to_string(),
            format!("{}", u64::MAX),
            "9".repeat(200),
        ] {
            let refused = content_length(&value).expect_err("this is too big");
            assert_eq!(refused.status, 413, "`{value}` is too big, not malformed");
        }
    }

    /// A `fetch` made by a run with nothing left does not open a connection
    /// at all, let alone wait thirty seconds on one.
    #[test]
    fn a_fetch_with_no_time_left_is_refused_before_it_connects() {
        let answer = fetch_over_tcp("http://127.0.0.1:1/", Some(Duration::ZERO))
            .expect_err("a run with no time left cannot fetch");
        assert_eq!(
            answer,
            "http: the run ran out of time before 127.0.0.1:1 could be asked"
        );
    }
}
