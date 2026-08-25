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
//! safepoints, and stopping the run stops the server.
//!
//! Three implementations ship. [`Http::real`] speaks HTTP/1.1 over TCP, and
//! is deliberately small: one request per connection, `Connection: close`, no
//! keep-alive, no chunked transfer, and a listener that binds loopback only,
//! because granting `http` should not publish a port to the network the
//! machine is on. [`Http::recorded`] is the fake: `fetch` answers from a
//! table of canned bodies and a listener replays a scripted queue of
//! requests, so a program that serves is testable without a socket.
//! [`Http::denied`] refuses everything and says why.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cove_sema::Capability;

use crate::error::RuntimeError;
use crate::host::{HostApi, Reentry, ResourceHandle};
use crate::schema::{Effect, FieldSchema, HostType, OperationSchema, ResourceSchema, TypeSchema};
use crate::value::{EnumValue, StructValue, Value};

/// How long the real host waits for one client to send its request line.
///
/// A connection that opens and says nothing would otherwise hold `handle`
/// open for as long as the peer chose to keep it.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The types `http` declares.
///
/// All four are ordinary data: a request and a response are what crossed the
/// wire, a method is one of two names, and a route pairs them with the
/// callback that answers it. A `handler` is [`HostType::Any`] because the
/// host never looks inside it — it stores the value and calls it.
static HTTP_TYPES: &[TypeSchema] = &[
    TypeSchema {
        name: "Method",
        cases: &["Get", "Post"],
        fields: &[],
    },
    TypeSchema {
        name: "Request",
        cases: &[],
        fields: &[
            FieldSchema {
                name: "method",
                ty: HostType::Named("http.Method"),
            },
            FieldSchema {
                name: "path",
                ty: HostType::String,
            },
            FieldSchema {
                name: "body",
                ty: HostType::String,
            },
        ],
    },
    TypeSchema {
        name: "Response",
        cases: &[],
        fields: &[
            FieldSchema {
                name: "status",
                ty: HostType::Int,
            },
            FieldSchema {
                name: "body",
                ty: HostType::String,
            },
        ],
    },
    TypeSchema {
        name: "Route",
        cases: &[],
        fields: &[
            FieldSchema {
                name: "method",
                ty: HostType::Named("http.Method"),
            },
            FieldSchema {
                name: "path",
                ty: HostType::String,
            },
            FieldSchema {
                name: "handler",
                ty: HostType::Any,
            },
        ],
    },
];

/// The operations `http` exposes.
///
/// `listen` is a reversible write: it takes a port from the machine, and
/// `close` gives it back. `fetch` reads, since a `GET` is what it sends and
/// nothing outside the run is different afterwards. `json` touches nothing at
/// all — it is a constructor the host owns because the host owns the
/// encoding.
static HTTP_SCHEMA: &[OperationSchema] = &[
    OperationSchema {
        name: "fetch",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Result(&HostType::String, &HostType::Error),
        capability: "http",
        effect: Effect::Read,
        cancellable: true,
        recordable: true,
        result_is_task_safe: true,
    },
    OperationSchema {
        name: "json",
        params: &[HostType::Int, HostType::Any],
        variadic: false,
        result: HostType::Named("http.Response"),
        capability: "http",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    },
    OperationSchema {
        name: "listen",
        params: &[HostType::Int],
        variadic: false,
        result: HostType::Result(&HostType::Named("http.Server"), &HostType::Error),
        capability: "http",
        effect: Effect::ReversibleWrite,
        cancellable: false,
        // A handle is a name, so recording one records the name. A replay
        // hands the same name back and answers the calls made on it from the
        // trace as well.
        recordable: true,
        result_is_task_safe: true,
    },
];

/// What a `http.Server` handle answers.
///
/// The listener lives behind a lock the host owns, so two tasks may both hold
/// the handle and take turns accepting: the resource is task-safe, and the
/// schema is where it says so.
static HTTP_RESOURCES: &[ResourceSchema] = &[ResourceSchema {
    name: "Server",
    task_safe: true,
    operations: &[
        OperationSchema {
            name: "port",
            params: &[],
            variadic: false,
            result: HostType::Int,
            capability: "http",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "handle",
            params: &[HostType::Array(&HostType::Named("http.Route"))],
            variadic: false,
            result: HostType::Result(&HostType::Bool, &HostType::Error),
            capability: "http",
            // A response that has reached a client cannot be taken back.
            effect: Effect::IrreversibleWrite,
            cancellable: true,
            // The answer is whether a request arrived, which is a fact about
            // the run and not about the handler that ran inside it. Replaying
            // it reproduces the shape of the loop; the handler runs for real
            // either way, because it is the program's own code.
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "close",
            params: &[],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "http",
            effect: Effect::ReversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
}];

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
    /// Canned bodies for `fetch`, and a scripted request queue for a
    /// listener to replay.
    Recorded {
        bodies: BTreeMap<String, String>,
        requests: Vec<ScriptedRequest>,
        /// What every handled request answered, in order, so a test can read
        /// back what the program served.
        served: Arc<Mutex<Vec<String>>>,
    },
    /// A host with no network at all.
    Denied,
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

    /// A fake that answers `fetch` from `bodies` and lets a listener replay
    /// `requests`, for tests.
    ///
    /// The key is the URL exactly as the program writes it. This is a
    /// recorded answer, not a client: a fake that resolved a host name would
    /// be reaching the network the grant was supposed to describe.
    pub fn recorded(bodies: BTreeMap<String, String>, requests: Vec<ScriptedRequest>) -> Self {
        Http::with_source(HttpSource::Recorded {
            bodies,
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
            &HTTP_RESOURCES[0],
            id,
        ))))
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Listener>> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `GET url`, answering the body a `2xx` carried.
    fn fetch(&self, url: &str) -> Value {
        match &self.source {
            HttpSource::Real => match fetch_over_tcp(url) {
                Ok(body) => Value::ok(Value::Str(body.into())),
                Err(message) => Value::err(Value::error(message)),
            },
            HttpSource::Recorded { bodies, .. } => match bodies.get(url) {
                Some(body) => Value::ok(Value::Str(body.as_str().into())),
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
    /// `false` means the listener has nothing more to serve, which is what
    /// ends the loop the program wrote around this call. A real listener
    /// waits, so it answers `false` only once it has been closed.
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
            Next::Real(listener) => match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                    match read_request(&stream) {
                        Ok((method, path, body)) => (
                            ScriptedRequest {
                                method: method_case(&method),
                                path,
                                body,
                            },
                            Some(stream),
                        ),
                        Err(message) => {
                            let _ = write_response(&stream, 400, &json_string(&message));
                            return Ok(Value::ok(Value::Bool(true)));
                        }
                    }
                }
                Err(e) => {
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
        Value::Enum(result) if &*result.type_name == "Result" => match &*result.case {
            "Ok" => response_of(result.payload.first().unwrap_or(&Value::Unit)),
            _ => Ok((
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
    fn name(&self) -> &str {
        "http"
    }

    fn capability(&self) -> Capability {
        Capability::new("http")
    }

    fn schema(&self) -> &[OperationSchema] {
        HTTP_SCHEMA
    }

    fn types(&self) -> &[TypeSchema] {
        HTTP_TYPES
    }

    fn resources(&self) -> &[ResourceSchema] {
        HTTP_RESOURCES
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "fetch" => {
                let [Value::Str(url)] = args.as_slice() else {
                    return Err(RuntimeError::new(
                        "`http.fetch` takes one `String` argument",
                    ));
                };
                Ok(self.fetch(url))
            }
            "json" => {
                let [Value::Int(status), body] = args.as_slice() else {
                    return Err(RuntimeError::new(
                        "`http.json` takes an `Int` status and a value to encode",
                    ));
                };
                Ok(response(*status, &json_of(body)))
            }
            "listen" => {
                let [Value::Int(port)] = args.as_slice() else {
                    return Err(RuntimeError::new("`http.listen` takes one `Int` argument"));
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
                    return Err(RuntimeError::new(
                        "`http.Server.handle` takes one `Array<http.Route>` argument",
                    ));
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
    Value::Struct(Box::new(StructValue {
        type_name: "http.Response".into(),
        fields: vec![
            ("status".into(), Value::Int(status)),
            ("body".into(), Value::Str(body.into())),
        ],
    }))
}

/// `http.Request(method: ..., path: ..., body: ...)`.
fn request(method: &str, path: &str, body: &str) -> Value {
    Value::Struct(Box::new(StructValue {
        type_name: "http.Request".into(),
        fields: vec![
            ("method".into(), method_value(method)),
            ("path".into(), Value::Str(path.into())),
            ("body".into(), Value::Str(body.into())),
        ],
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

/// Sends one `GET` and reads the body a `2xx` carried.
fn fetch_over_tcp(url: &str) -> Result<String, String> {
    let (authority, path) = split_url(url)?;
    let mut stream = TcpStream::connect(&authority)
        .map_err(|e| format!("http: cannot connect to {authority}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("http: {e}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("http: cannot send to {authority}: {e}"))?;
    let mut answer = Vec::new();
    stream
        .read_to_end(&mut answer)
        .map_err(|e| format!("http: cannot read from {authority}: {e}"))?;
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
    if !(200..300).contains(&status) {
        return Err(format!("http: {url} answered {status}"));
    }
    Ok(body.to_string())
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
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// Reads one HTTP/1.1 request from `stream`.
fn read_request(stream: &TcpStream) -> Result<(String, String, String), String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("http: cannot read the request line: {e}"))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        let read = reader
            .read_line(&mut header)
            .map_err(|e| format!("http: cannot read a header: {e}"))?;
        if read == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|e| format!("http: cannot read the body: {e}"))?;
    }
    let path = target.split('?').next().unwrap_or("/").to_string();
    Ok((method, path, String::from_utf8_lossy(&body).into_owned()))
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
