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

use cove_schema::builtins::RESULT;
use cove_sema::Capability;

use crate::error::RuntimeError;
use crate::host::{HostApi, Reentry, ResourceHandle};
use crate::schema::{ModuleSchema, OperationSchema, ResourceSchema, TypeSchema};
use crate::value::{EnumValue, StructValue, Value};

/// How long the real host waits for one client to send its request line.
///
/// A connection that opens and says nothing would otherwise hold `handle`
/// open for as long as the peer chose to keep it.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

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
            &SCHEMA.resources[0],
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
    /// ends the loop the program wrote around this call. Only a fake listener
    /// ever says it: a real one waits for a connection, so a program serving
    /// against one runs until the run itself is stopped.
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
    fn name(&self) -> &str {
        "http"
    }

    fn capability(&self) -> Capability {
        Capability::new("http")
    }

    fn schema(&self) -> &[OperationSchema] {
        SCHEMA.operations
    }

    fn types(&self) -> &[TypeSchema] {
        SCHEMA.types
    }

    fn resources(&self) -> &[ResourceSchema] {
        SCHEMA.resources
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "fetch" => {
                let [Value::Str(url)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(self.fetch(url))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Cancellation;
    use crate::host::NoReentry;
    use crate::value::MapKey;
    use std::rc::Rc;

    fn err_message(value: Value) -> String {
        match value.err_payload() {
            Some(payload) => payload.first().map(ToString::to_string).unwrap_or_default(),
            None => panic!("expected `Err(...)`, found {value}"),
        }
    }

    fn is_ok(value: &Value) -> bool {
        value.is_ok()
    }

    fn ok_str(value: Value) -> String {
        match value.ok_payload() {
            Some(payload) => match payload.first() {
                Some(Value::Str(s)) => s.to_string(),
                other => panic!("expected `Ok(String)`, found {other:?}"),
            },
            None => panic!("expected `Ok(...)`, found {value}"),
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
        Value::Struct(Box::new(StructValue {
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
    /// handler into Cove code.
    struct StubReentry {
        calls: usize,
        respond: Box<dyn FnMut() -> Result<Value, RuntimeError>>,
    }

    impl StubReentry {
        fn new(respond: impl FnMut() -> Result<Value, RuntimeError> + 'static) -> Self {
            StubReentry {
                calls: 0,
                respond: Box::new(respond),
            }
        }
    }

    impl Reentry for StubReentry {
        fn call(&mut self, _callee: &Value, _args: Vec<Value>) -> Result<Value, RuntimeError> {
            self.calls += 1;
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
            false
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
    fn a_recorded_fetch_answers_its_body() {
        let http = Http::recorded(
            BTreeMap::from([("http://example.com/".to_string(), "hello".to_string())]),
            Vec::new(),
        );
        let answer = http
            .call("fetch", vec![Value::Str("http://example.com/".into())])
            .unwrap();
        assert_eq!(ok_str(answer), "hello");
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
        let payload = Value::Struct(Box::new(StructValue {
            type_name: "demo.Point".into(),
            fields: vec![("x".into(), Value::Int(1)), ("y".into(), Value::Int(2))],
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

        assert_eq!(ok_str(answer), "hello from loopback");
    }

    #[test]
    fn a_real_fetch_reports_a_non_2xx_status() {
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

        assert_eq!(err_message(answer), format!("http: {url} answered 404"));
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
}
