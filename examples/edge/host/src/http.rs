//! Just enough HTTP/1.1 to be curl-able: one request per connection.
//!
//! A request line, headers, and a `Content-Length` body in; a status line,
//! three headers and a body out, with `Connection: close`. No chunked bodies,
//! no keep-alive, no TLS — a demo of the isolates behind it, not of a web
//! server, and std alone.

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

    /// The bytes on the wire.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.status,
            reason(self.status),
            self.content_type,
            self.body.len()
        )
        .into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        out
    }

    /// Writes the response and closes the write half.
    pub fn send(&self, stream: &mut TcpStream) -> io::Result<()> {
        stream.write_all(&self.to_bytes())?;
        stream.flush()?;
        let _ = stream.shutdown(std::net::Shutdown::Write);
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
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Reads one request from `stream`.
pub fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at;
        }
        if buffer.len() > MAX_REQUEST {
            return Err("request head too large".to_string());
        }
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before the request ended".to_string());
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buffer[..head_end]).map_err(|_| "head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("malformed request line `{request_line}`"));
    };
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad Content-Length `{}`", value.trim()))?;
            }
        }
    }
    if length > MAX_REQUEST {
        return Err("request body too large".to_string());
    }
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before the body ended".to_string());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    Ok(Request {
        method: method.to_string(),
        path: decode(path),
        query: query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) => (decode(k), decode(v)),
                None => (decode(pair), String::new()),
            })
            .collect(),
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// A blocking `GET` of `target` from `addr`: the status and the body.
///
/// What the tests and the load generator's bookkeeping use; the load itself
/// is driven without it, many sockets to a thread.
pub fn get(addr: impl std::net::ToSocketAddrs, target: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(request_bytes("GET", target, "").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    parse_response(&raw)
}

/// A request as the bytes a client writes.
pub fn request_bytes(method: &str, target: &str, body: &str) -> String {
    format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A whole response, read to the end of the connection: its status and body.
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
