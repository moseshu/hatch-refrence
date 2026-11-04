//! Minimal HTTP/1.1 pieces for the inference proxy: request parsing
//! and chunked streaming responses.
//!
//! Requests are read by `Content-Length` only (the agent's provider
//! client always sends one); chunked request bodies are rejected.
//! Responses are streamed back with `Transfer-Encoding: chunked` so
//! a slow model stream reaches the client chunk by chunk instead of
//! being buffered whole.

use std::io::{BufRead, Write};

/// Largest request body accepted (8 MiB — model prompts with long
/// histories are far bigger than the 1 MiB MCP cap).
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// One parsed HTTP request.
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
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
}

/// Parse one HTTP/1.1 request from `reader`.
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
    let (path, query) = match raw_path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (raw_path.clone(), String::new()),
    };
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    if headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunked request bodies are not supported",
        ));
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
        query,
        headers,
        body,
    })
}

/// Write response headers for a chunked body. The caller then writes
/// chunks with [`write_chunk`] and finishes with [`end_chunks`].
pub fn write_chunked_head(
    stream: &mut dyn Write,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

/// Write one chunk of a chunked body, flushing so a slow stream
/// reaches the client promptly.
pub fn write_chunk(stream: &mut dyn Write, chunk: &[u8]) -> std::io::Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())?;
    stream.write_all(chunk)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

/// Terminate a chunked body.
pub fn end_chunks(stream: &mut dyn Write) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

/// Write one complete (non-streaming) response with `Content-Length`.
pub fn write_response(
    stream: &mut dyn Write,
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
    stream: &mut dyn Write,
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    write_response(
        stream,
        status,
        reason,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_request_and_rejects_chunked_and_oversized() {
        let req = parse_request(&mut Cursor::new(
            b"POST /responses HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
        ))
        .unwrap();
        assert_eq!(
            (req.method.as_str(), req.path.as_str()),
            ("POST", "/responses")
        );
        assert_eq!(req.body, b"{}");
        assert!(parse_request(&mut Cursor::new(
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice()
        ))
        .is_err());
        let big = format!(
            "POST /x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(parse_request(&mut Cursor::new(big.as_bytes())).is_err());
    }

    #[test]
    fn chunked_round_trip() {
        let mut out = Vec::new();
        write_chunked_head(
            &mut out,
            200,
            "OK",
            &[("Content-Type".into(), "text/event-stream".into())],
        )
        .unwrap();
        write_chunk(&mut out, b"data: 1\n\n").unwrap();
        write_chunk(&mut out, b"data: 2\n\n").unwrap();
        end_chunks(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Transfer-Encoding: chunked\r\n"));
        assert!(text.contains("\r\n9\r\ndata: 1\n\n\r\n"));
        assert!(text.ends_with("0\r\n\r\n"));
    }
}
