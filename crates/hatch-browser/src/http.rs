//! Minimal HTTP/1.1 client for the local DevTools endpoints (`/json/...`).
//! Only ever talks to 127.0.0.1, so no proxy and no TLS.

use crate::error::{BResult, BrowserError};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// `METHOD path` against `127.0.0.1:port`; returns (status, body).
pub fn local_request(port: u16, method: &str, path: &str, timeout: Duration) -> BResult<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| BrowserError::Unavailable(format!("DevTools 端口 {port}：{e}")))?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    s.write_all(req.as_bytes()).map_err(|e| BrowserError::Unavailable(e.to_string()))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    // The DevTools server may keep the connection open: stop at Content-Length.
    let (head, body) = loop {
        let n = s.read(&mut chunk).map_err(|e| BrowserError::Unavailable(format!("读取 DevTools 响应：{e}")))?;
        if n > 0 {
            buf.extend_from_slice(&chunk[..n]);
        }
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let want = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)));
            let have = buf.len() - pos - 4;
            if n == 0 || want.is_some_and(|w| have >= w) {
                let end = want.map(|w| pos + 4 + w.min(have)).unwrap_or(buf.len());
                break (head, String::from_utf8_lossy(&buf[pos + 4..end]).to_string());
            }
        } else if n == 0 {
            return Err(BrowserError::Protocol("DevTools 响应格式错误".into()));
        }
    };
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| BrowserError::Protocol("DevTools 响应没有状态码".into()))?;
    Ok((status, body.to_string()))
}

/// Parse a JSON body, requiring HTTP 200.
pub fn local_json(port: u16, method: &str, path: &str, timeout: Duration) -> BResult<serde_json::Value> {
    let (status, body) = local_request(port, method, path, timeout)?;
    if status != 200 {
        return Err(BrowserError::Protocol(format!("{method} {path} → HTTP {status}: {}", body.chars().take(200).collect::<String>())));
    }
    serde_json::from_str(&body).map_err(|e| BrowserError::Protocol(format!("{path}: {e}")))
}
