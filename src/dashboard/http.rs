//! Enough HTTP/1.1 for the loopback dashboard: request parsing and responses, the loopback checks on Host and Origin, and escaping for URLs, HTML, and JSON.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Request heads are cut off at this many bytes.
const MAX_HEAD: usize = 16 * 1024;

/// A parsed request line and headers.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
}

impl Request {
    /// Parse the head of a request. Header names are lowercased.
    pub fn parse(head: &str) -> Option<Request> {
        let mut lines = head.split("\r\n").flat_map(|l| l.split('\n'));
        let mut first = lines.next()?.split_whitespace();
        let method = first.next()?.to_owned();
        let target = first.next().unwrap_or("/");
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_owned(), q.to_owned()),
            None => (target.to_owned(), String::new()),
        };
        let headers = lines.take_while(|l| !l.is_empty()).filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        }).collect();
        Some(Request { method, path, query, headers })
    }

    /// Read a request head from the stream. The client has five seconds in total to send the whole head, however slowly it sends.
    pub fn read(stream: &mut TcpStream) -> Option<Request> {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = Vec::with_capacity(2048);
        let mut chunk = [0u8; 2048];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let _ = stream.set_read_timeout(Some(remaining));
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n") || buf.len() >= MAX_HEAD {
                        break;
                    }
                }
            }
        }
        Request::parse(&String::from_utf8_lossy(&buf))
    }

    /// The first header with this lowercase name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The decoded value of a query parameter, or None if the query lacks it.
    pub fn param(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| url_decode(v))
        })
    }
}

/// A response ready to write.
pub struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub extra: Vec<String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: &'static str, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
        Response { status, content_type, extra: Vec::new(), body: body.into() }
    }

    pub fn html(body: String) -> Response {
        Response::new("200 OK", "text/html; charset=utf-8", body)
    }

    pub fn json(body: String) -> Response {
        Response::new("200 OK", "application/json", body)
    }

    pub fn text(status: &'static str, body: impl Into<String>) -> Response {
        Response::new(status, "text/plain; charset=utf-8", body.into())
    }

    /// A 303 redirect to the main page, sent by the control routes after they act.
    pub fn redirect_home() -> Response {
        let mut r = Response::new("303 See Other", "text/plain", "");
        r.extra.push("Location: /".into());
        r
    }

    pub fn not_found() -> Response {
        Response::text("404 Not Found", "not found")
    }

    pub fn method_not_allowed(allow: &str) -> Response {
        let mut r = Response::text("405 Method Not Allowed", format!("method not allowed; use {allow}"));
        r.extra.push(format!("Allow: {allow}"));
        r
    }

    /// Write the response with `Connection: close`, since the server answers a single request per connection.
    pub fn write(&self, stream: &mut TcpStream) {
        let mut head = format!("HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n", self.status, self.content_type, self.body.len());
        for h in &self.extra {
            head.push_str(h);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&self.body);
        let _ = stream.flush();
    }
}

/// True if a Host value is `localhost`, `127.0.0.1`, or `::1`, with or without a port.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    matches!(name.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1")
}

/// Check that a POST comes from the dashboard itself. The Host must be loopback, to block DNS rebinding. If the browser sends an Origin, it must be a loopback address on the dashboard's port. If it sends Sec-Fetch-Site, the request must come from the dashboard page or straight from the address bar.
pub fn post_allowed(req: &Request, port: u16) -> Result<(), String> {
    match req.header("host") {
        Some(h) if is_loopback_host(h) => {}
        Some(h) => return Err(format!("POST refused: Host {h:?} is not loopback")),
        None => return Err("POST refused: no Host header".into()),
    }
    if let Some(origin) = req.header("origin") {
        let ok = origin.strip_prefix("http://").is_some_and(|rest| is_loopback_host(rest) && rest.rsplit_once(':').is_some_and(|(_, p)| p == port.to_string()));
        if !ok {
            return Err(format!("POST refused: Origin {origin:?} is not this dashboard"));
        }
    }
    if let Some(site) = req.header("sec-fetch-site") {
        if !matches!(site, "same-origin" | "none") {
            return Err(format!("POST refused: Sec-Fetch-Site {site:?}"));
        }
    }
    Ok(())
}

/// Escape `&`, `<`, `>`, and `"` for HTML text and attributes.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Decode a query value. The loop works on bytes instead of chars, so a stray `%` before a multibyte character leaves that character intact.
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => { out.push(b); i += 2; }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a query value.
pub fn url_encode(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

/// Quote a string for JSON.
pub fn json_str(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_decode_works_on_bytes() {
        assert_eq!(url_decode("a+b%20c"), "a b c");
        assert_eq!(url_decode("%C3%A9"), "é");
        assert_eq!(url_decode("%é"), "%é");
        assert_eq!(url_decode("%zz"), "%zz");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn requests_parse_headers_and_query() {
        let req = Request::parse("POST /gate?rel=a%20b&to=closed HTTP/1.1\r\nHost: localhost:18516\r\nOrigin: http://localhost:18516\r\n\r\n").unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/gate");
        assert_eq!(req.param("rel").as_deref(), Some("a b"));
        assert_eq!(req.param("to").as_deref(), Some("closed"));
        assert_eq!(req.header("host"), Some("localhost:18516"));
        assert!(Request::parse("").is_none());
    }

    #[test]
    fn posts_need_a_loopback_host_and_a_matching_origin() {
        let parse = |head: &str| Request::parse(head).unwrap();
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\n\r\n"), 18516).is_ok());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: 127.0.0.1:18516\r\nOrigin: http://127.0.0.1:18516\r\n\r\n"), 18516).is_ok());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: [::1]:18516\r\nOrigin: http://[::1]:18516\r\nSec-Fetch-Site: same-origin\r\n\r\n"), 18516).is_ok());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\nOrigin: https://evil.example\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\nOrigin: null\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\nOrigin: http://localhost:1\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\nSec-Fetch-Site: cross-site\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: evil.example:18516\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\n\r\n"), 18516).is_err());
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_str("plain"), "\"plain\"");
        assert_eq!(json_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }
}
