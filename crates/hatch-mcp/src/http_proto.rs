//! Low-level HTTP/1.1 pieces for the MCP streamable-HTTP server:
//! request parsing, response writing, bearer-token comparison and
//! session-id generation. Nothing here knows about MCP or the tool
//! registry — see [`crate::http_server`] and [`crate::http_routes`].

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde_json::Value;

/// Largest request body accepted (1 MiB). Larger bodies get 413.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// One parsed HTTP request (request line + headers + body).
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn session_id(&self) -> Option<&str> {
        self.header("mcp-session-id")
    }

    /// First 8 chars of the session id, for logs (never the full id).
    pub fn session_short(&self) -> String {
        self.session_id()
            .map(|s| s.chars().take(8).collect())
            .unwrap_or_default()
    }
}

/// Parse one HTTP/1.1 request from `reader`. Bodies are read by
/// `Content-Length` only (no chunked encoding); a missing length means
/// an empty body. Bodies over [`MAX_BODY_BYTES`] are rejected.
pub fn parse_request(reader: &mut impl BufRead) -> std::io::Result<HttpRequest> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty request line",
        ));
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_path = parts.next().unwrap_or("").to_string();
    if method.is_empty() || raw_path.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    }
    let path = raw_path.split('?').next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request body too large",
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

/// Generate a random session id: 16 bytes from `/dev/urandom`, hex.
/// Falls back to time + pid + counter when urandom is unavailable.
pub fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut bytes).is_ok() {
            return bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:x}-{n:x}-{}", std::process::id())
}

/// Constant-time bearer-token comparison (no early exit on the first
/// differing byte; the length check happens before comparing).
pub fn bearer_ok(req: &HttpRequest, token: &str) -> bool {
    let Some(auth) = req.header("authorization") else {
        return false;
    };
    let Some(given) = auth.strip_prefix("Bearer ") else {
        return false;
    };
    let (a, b) = (given.as_bytes(), token.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Write one HTTP/1.1 response (headers + body, `Connection: close`).
pub fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Write a JSON body with the given status.
pub fn write_json(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> std::io::Result<()> {
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    headers.extend_from_slice(extra);
    write_response(stream, status, reason, &headers, body.as_bytes())
}

/// One JSON-RPC error object, serialized.
pub fn rpc_error(id: &Value, code: i64, message: &str) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(text: &str) -> std::io::Result<HttpRequest> {
        parse_request(&mut Cursor::new(text.as_bytes()))
    }

    #[test]
    fn parses_method_path_headers_and_body() {
        let req = parse(
            "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/mcp");
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.body, b"{}");
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_strips_query() {
        let req = parse("GET /mcp?x=1 HTTP/1.1\r\nMcp-Session-Id: abcdefgh1234\r\n\r\n").unwrap();
        assert_eq!(req.path, "/mcp");
        assert_eq!(req.session_id(), Some("abcdefgh1234"));
        assert_eq!(req.session_short(), "abcdefgh");
    }

    #[test]
    fn empty_request_and_oversized_body_are_rejected() {
        assert!(parse("").is_err());
        assert!(parse("POST /mcp HTTP/1.1\r\n\r\n").is_ok()); // no body
        let big = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(parse(&big).is_err());
    }

    #[test]
    fn bearer_comparison_rejects_wrong_and_missing_tokens() {
        let req = parse("POST /mcp HTTP/1.1\r\nAuthorization: Bearer secret\r\n\r\n").unwrap();
        assert!(bearer_ok(&req, "secret"));
        assert!(!bearer_ok(&req, "other"));
        assert!(!bearer_ok(&req, "secre")); // different length
        let no_auth = parse("POST /mcp HTTP/1.1\r\n\r\n").unwrap();
        assert!(!bearer_ok(&no_auth, "secret"));
    }

    #[test]
    fn session_ids_are_unique() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32); // 16 random bytes, hex
    }
}
