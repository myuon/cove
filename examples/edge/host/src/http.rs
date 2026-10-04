//! Just enough HTTP/1.1 to be curl-able, with keep-alive.
//!
//! A request line, headers, and a `Content-Length` body in; a status line,
//! three headers and a body out. A connection is kept open after a response
//! when the client spoke HTTP/1.1 and did not say `Connection: close` (or
//! spoke 1.0 and said `Connection: keep-alive`), and the server has not
//! chosen to close it; the response says which with its own `Connection`
//! header. No chunked bodies, no TLS — a demo of the isolates behind it, not
//! of a web server, and std alone.

use std::io::{self, Read, Write};
use std::net::TcpStream;

/// The most a request's head and body may be, together.
const MAX_REQUEST: usize = 64 * 1024;

/// One parsed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// The path, percent-decoded, without the query.
    pub path: String,
    /// The query's pairs, percent-decoded, in the order they were written.
    pub query: Vec<(String, String)>,
    pub body: String,
    /// Whether the client asked for the connection to stay open: HTTP/1.1
    /// without `Connection: close`, or HTTP/1.0 with `Connection:
    /// keep-alive`.
    pub keep_alive: bool,
}

/// One response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

impl Response {
    /// A `text/plain` response.
    pub fn text(status: u16, body: impl Into<String>) -> Response {
        Response {
            status,
            content_type: "text/plain".to_string(),
            body: body.into(),
        }
    }

    /// The bytes on the wire, saying whether the connection stays open.
    pub fn to_bytes(&self, keep_alive: bool) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
            self.status,
            reason(self.status),
            self.content_type,
            self.body.len(),
            if keep_alive { "keep-alive" } else { "close" },
        )
        .into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        out
    }

    /// Writes the response, and closes the write half unless the connection
    /// is kept open for another request.
    pub fn send(&self, stream: &mut TcpStream, keep_alive: bool) -> io::Result<()> {
        stream.write_all(&self.to_bytes(keep_alive))?;
        stream.flush()?;
        if !keep_alive {
            let _ = stream.shutdown(std::net::Shutdown::Write);
        }
        Ok(())
    }
}

/// The reason phrase for the statuses this server answers.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Whether `buffer` already holds a whole request head, so that the next
/// request can be read without waiting for the socket.
pub fn holds_a_head(buffer: &[u8]) -> bool {
    find(buffer, b"\r\n\r\n").is_some()
}

/// Reads one request from `stream`, starting with whatever `buffer` holds
/// from the last read on the same connection, and leaves in `buffer` what
/// was read past the end of it — the start of a pipelined next request.
///
/// `Ok(None)` is a connection the client closed between requests, which on
/// a kept-alive connection is how it ends and not an error.
pub fn read_request(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<Option<Request>, String> {
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = find(buffer, b"\r\n\r\n") {
            break at;
        }
        if buffer.len() > MAX_REQUEST {
            return Err("request head too large".to_string());
        }
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            if buffer.is_empty() {
                return Ok(None);
            }
            return Err("connection closed before the request ended".to_string());
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buffer[..head_end]).map_err(|_| "head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("malformed request line `{request_line}`"));
    };
    let mut length = 0usize;
    let mut keep_alive = version == "HTTP/1.1";
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim(), value.trim());
            if name.eq_ignore_ascii_case("content-length") {
                length = value
                    .parse()
                    .map_err(|_| format!("bad Content-Length `{value}`"))?;
            } else if name.eq_ignore_ascii_case("connection") {
                for token in value.split(',').map(str::trim) {
                    if token.eq_ignore_ascii_case("close") {
                        keep_alive = false;
                    } else if token.eq_ignore_ascii_case("keep-alive") {
                        keep_alive = true;
                    }
                }
            }
        }
    }
    if length > MAX_REQUEST {
        return Err("request body too large".to_string());
    }
    let (method, target) = (method.to_string(), target.to_string());
    let body_start = head_end + 4;
    while buffer.len() < body_start + length {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before the body ended".to_string());
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buffer[body_start..body_start + length]).into_owned();
    buffer.drain(..body_start + length);
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target.as_str(), ""),
    };
    Ok(Some(Request {
        method,
        path: decode(path),
        query: query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) => (decode(k), decode(v)),
                None => (decode(pair), String::new()),
            })
            .collect(),
        body,
        keep_alive,
    }))
}

/// A blocking `GET` of `target` from `addr` on a connection of its own,
/// closed after it: the status and the body.
///
/// What the tests and the load generator's bookkeeping use; the load itself
/// is driven without it, many sockets to a thread.
pub fn get(addr: impl std::net::ToSocketAddrs, target: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(request_bytes("GET", target, "", false).as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    parse_response(&raw)
}

/// A request as the bytes a client writes, asking for the connection to be
/// kept open or closed after it.
pub fn request_bytes(method: &str, target: &str, body: &str, keep_alive: bool) -> String {
    format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n{body}",
        body.len(),
        if keep_alive { "keep-alive" } else { "close" },
    )
}

/// A whole response — read to the end of the connection, or cut at
/// [`response_length`] — as its status and body.
pub fn parse_response(raw: &[u8]) -> Result<(u16, String), String> {
    let head_end = find(raw, b"\r\n\r\n").ok_or("the response has no head")?;
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no status in `{head}`"))?;
    Ok((
        status,
        String::from_utf8_lossy(&raw[head_end + 4..]).into_owned(),
    ))
}

/// How many bytes of `raw` are its first response, once all of them have
/// arrived: the head and `Content-Length` bytes of body. `None` while it is
/// incomplete, or for a response with no `Content-Length`, which ends with
/// its connection.
pub fn response_length(raw: &[u8]) -> Option<usize> {
    let head_end = find(raw, b"\r\n\r\n")?;
    let length = header(&raw[..head_end], "content-length")?
        .parse::<usize>()
        .ok()?;
    let total = head_end + 4 + length;
    (raw.len() >= total).then_some(total)
}

/// Whether a response head says the server will close the connection.
pub fn closes(raw: &[u8]) -> bool {
    let head_end = find(raw, b"\r\n\r\n").unwrap_or(raw.len());
    header(&raw[..head_end], "connection").is_some_and(|value| value.eq_ignore_ascii_case("close"))
}

/// A header's value, from a response or request head.
fn header(head: &[u8], name: &str) -> Option<String> {
    String::from_utf8_lossy(head)
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
}

/// An `http://` URL, taken apart: what an outbound fetch connects to and
/// asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    /// The host as written — a name or an address — which is what a
    /// tenant's allowlist names.
    pub host: String,
    pub port: u16,
    /// The path and query, starting with `/`.
    pub target: String,
}

impl std::fmt::Display for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http://{}:{}{}", self.host, self.port, self.target)
    }
}

/// Parses `http://host[:port][/path[?query]]`. Anything else — `https`, which
/// would need TLS, or no scheme at all — is refused with the reason.
pub fn parse_url(text: &str) -> Result<Url, String> {
    let Some(rest) = text.strip_prefix("http://") else {
        if text.starts_with("https://") {
            return Err(format!(
                "cannot fetch `{text}`: https needs TLS, which this server does not speak"
            ));
        }
        return Err(format!("cannot fetch `{text}`: not an `http://` URL"));
    };
    let (authority, target) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse()
                .map_err(|_| format!("cannot fetch `{text}`: bad port `{port}`"))?,
        ),
        None => (authority, 80),
    };
    if host.is_empty() || host.contains('@') {
        return Err(format!("cannot fetch `{text}`: no host"));
    }
    Ok(Url {
        host: host.to_ascii_lowercase(),
        port,
        target: target.to_string(),
    })
}

/// The most of a fetched response this server reads.
const MAX_FETCHED: usize = 1024 * 1024;

/// A blocking `GET` of `url`, on a connection of its own: the status and the
/// body.
///
/// `connected` is handed the connection as soon as it is open, before the
/// request is written — which is how the fetch pool keeps a handle that can
/// abort a fetch nobody wants any more (`TcpStream::shutdown` from another
/// thread ends the read below at once). It answers `false` to give up before
/// anything is sent.
pub fn fetch(
    url: &Url,
    connected: impl FnOnce(&TcpStream) -> bool,
) -> Result<(u16, String), String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve `{}`: {e}", url.host))?
        .collect();
    // Every address in turn: `localhost` is `::1` before `127.0.0.1` on
    // macOS, and a server listening on one of them is still `localhost`.
    let mut last = format!("`{}` has no address", url.host);
    let mut connected_to = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(5)) {
            Ok(stream) => {
                connected_to = Some(stream);
                break;
            }
            Err(e) => last = format!("cannot connect to `{url}`: {e}"),
        }
    }
    let mut stream = connected_to.ok_or(last)?;
    if !connected(&stream) {
        return Err(format!("the fetch of `{url}` was abandoned"));
    }
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: cove-edge\r\nConnection: close\r\n\r\n",
        url.target, url.host, url.port
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("cannot send to `{url}`: {e}"))?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(length) = response_length(&raw) {
            raw.truncate(length);
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(format!("reading `{url}`: {e}")),
        }
        if raw.len() > MAX_FETCHED {
            return Err(format!("`{url}` answered more than {MAX_FETCHED} bytes"));
        }
    }
    if raw.is_empty() {
        return Err(format!("`{url}` closed the connection without answering"));
    }
    parse_response(&raw)
}

/// A client that keeps one connection open across requests, as a browser or
/// a reverse proxy would: what the tests use to watch keep-alive work.
pub struct Client {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Client {
    pub fn connect(addr: impl std::net::ToSocketAddrs) -> Result<Client, String> {
        let stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;
        Ok(Client {
            stream,
            buffer: Vec::new(),
        })
    }

    /// Sends `GET target` on the open connection, asking to keep it open
    /// unless `close`, and reads exactly one response: its status, its body,
    /// and whether the server said it will close the connection.
    pub fn get(&mut self, target: &str, close: bool) -> Result<(u16, String, bool), String> {
        self.stream
            .write_all(request_bytes("GET", target, "", !close).as_bytes())
            .map_err(|e| e.to_string())?;
        self.read_response()
    }

    /// Reads one response from the connection.
    pub fn read_response(&mut self) -> Result<(u16, String, bool), String> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(length) = response_length(&self.buffer) {
                let raw: Vec<u8> = self.buffer.drain(..length).collect();
                let (status, body) = parse_response(&raw)?;
                return Ok((status, body, closes(&raw)));
            }
            let n = self.stream.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("the server closed the connection".to_string());
            }
            self.buffer.extend_from_slice(&chunk[..n]);
        }
    }

    /// Whether the server has closed the connection: a read that answers
    /// end-of-file within `wait`.
    pub fn closed_within(&mut self, wait: std::time::Duration) -> bool {
        let _ = self.stream.set_read_timeout(Some(wait));
        let mut byte = [0u8; 1];
        let closed = matches!(self.stream.read(&mut byte), Ok(0));
        let _ = self
            .stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)));
        closed
    }

    /// The raw connection, for writing several requests at once.
    pub fn stream(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `%XX` and `+` decoded; anything malformed is kept as written.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' if at + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        at += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn decodes_a_query_value() {
        assert_eq!(decode("a%20b+c"), "a b c");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
    }
}
