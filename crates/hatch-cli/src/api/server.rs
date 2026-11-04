//! The product API on plain HTTP/1.1 (std `TcpListener`, no framework).
//!
//! One thread per connection, mirroring the MCP streamable-HTTP server:
//! fixed-window rate limit per client IP, bearer auth when a token is
//! configured, and method/path/status-only logging — never tokens, never
//! bodies. All replies are JSON (`{"error": ...}` on failure) except
//! the SSE chat stream.
//!
//! v1 is single-user: an `X-User-Id` header, when present, is accepted
//! and ignored. Mapping tokens to users is separate multi-tenant work
//! and is not pretended here.

use super::chat;
use super::routes;
use super::{ApiCtx, ApiError};
use hatch_core::{Error, Result};
use hatch_mcp::http_proto::{bearer_ok, parse_request, write_json};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Configuration for [`ApiServer`].
#[derive(Clone, Debug)]
pub struct ApiServerConfig {
    /// Address to listen on. Default `127.0.0.1:8787`.
    pub bind: SocketAddr,
    /// Bearer token required on every request except `/v1/health`,
    /// when set. `None` means no authentication — only safe on
    /// loopback, enforced by the CLI before the server starts.
    pub token: Option<String>,
    /// Fixed-window rate limit: requests per second per client IP.
    /// Default 20.
    pub rate_limit_per_sec: u32,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        ApiServerConfig {
            bind: "127.0.0.1:8787".parse().unwrap(),
            token: None,
            rate_limit_per_sec: 20,
        }
    }
}

/// The product API server. `ctx` is shared across connection threads.
pub struct ApiServer {
    config: ApiServerConfig,
    ctx: Arc<ApiCtx>,
    listener: TcpListener,
    /// Per-IP fixed-window counters: (window start, requests in window).
    limits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl ApiServer {
    /// Bind the listener. Does not serve yet; call [`ApiServer::run`].
    pub fn bind(config: ApiServerConfig, ctx: Arc<ApiCtx>) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind(config.bind)?;
        listener.set_nonblocking(true)?;
        Ok(Arc::new(ApiServer {
            config,
            ctx,
            listener,
            limits: Mutex::new(HashMap::new()),
        }))
    }

    /// The address actually bound (useful with port 0 in tests).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until `shutdown` is set. One thread per connection, via a
    /// scoped thread pool so `run` waits for in-flight connections.
    pub fn run(&self, shutdown: &AtomicBool) -> Result<()> {
        std::thread::scope(|scope| loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    scope.spawn(move || {
                        self.handle_conn(stream, addr.ip(), shutdown);
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if shutdown.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(Error::Connector(format!("api accept failed: {e}"))),
            }
        })
    }

    /// Fixed-window rate limit: at most `rate_limit_per_sec` requests
    /// per second per client IP.
    fn allow(&self, ip: IpAddr) -> bool {
        let mut limits = self.limits.lock().unwrap();
        let now = Instant::now();
        let entry = limits.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(1) {
            *entry = (now, 1);
            true
        } else if entry.1 < self.config.rate_limit_per_sec {
            entry.1 += 1;
            true
        } else {
            false
        }
    }

    /// One connection: parse, route, reply, log. Method/path/status
    /// only — never tokens or bodies.
    fn handle_conn(&self, stream: TcpStream, peer_ip: IpAddr, shutdown: &AtomicBool) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
        let mut reader = match stream.try_clone() {
            Ok(c) => BufReader::new(c),
            Err(_) => return,
        };
        let mut stream = stream;
        let (method, path, status) =
            self.dispatch(&mut reader, &mut stream, peer_ip, shutdown);
        eprintln!("[api] {method} {path} {status}");
    }

    /// Route one request. Returns method, path and status for the log
    /// line. Errors writing a reply are swallowed (the client is gone).
    fn dispatch(
        &self,
        reader: &mut BufReader<TcpStream>,
        stream: &mut TcpStream,
        peer_ip: IpAddr,
        shutdown: &AtomicBool,
    ) -> (&'static str, String, u16) {
        let req = match parse_request(reader) {
            Ok(r) => r,
            Err(_) => {
                let e = ApiError::bad_request("malformed request");
                let _ = write_json(stream, 400, "Bad Request", &[], &e.to_json().to_string());
                return ("?", String::new(), 400);
            }
        };
        let method = match req.method.as_str() {
            "GET" => "GET",
            "POST" => "POST",
            "PATCH" => "PATCH",
            "DELETE" => "DELETE",
            _ => "?",
        };
        let path = req.path.clone();
        if !path.starts_with("/v1/") {
            let e = ApiError::not_found("接口");
            let _ = write_json(stream, 404, "Not Found", &[], &e.to_json().to_string());
            return (method, path, 404);
        }
        if !self.allow(peer_ip) {
            let e = ApiError::new(429, "rate_limited", "请求过于频繁，请稍后再试。");
            let _ = write_json(
                stream,
                429,
                "Too Many Requests",
                &[("Retry-After", "1")],
                &e.to_json().to_string(),
            );
            return (method, path, 429);
        }
        // `/v1/health` is the unauthenticated liveness probe.
        if path != "/v1/health" {
            if let Some(token) = &self.config.token {
                if !bearer_ok(&req, token) {
                    let e = ApiError::new(401, "unauthorized", "缺少或无效的 bearer token。");
                    let _ = write_json(
                        stream,
                        401,
                        "Unauthorized",
                        &[("WWW-Authenticate", "Bearer")],
                        &e.to_json().to_string(),
                    );
                    return (method, path, 401);
                }
            }
        }
        // SSE chat owns the stream for the whole turn.
        if let Some(session) = routes::chat_stream_session(&req) {
            let status = chat::handle_message_stream(&self.ctx, stream, &req, &session, shutdown);
            return (method, path, status);
        }
        match routes::dispatch_json(&self.ctx, &req) {
            Ok((status, body)) => {
                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    _ => "OK",
                };
                let _ = write_json(stream, status, reason, &[], &body.to_string());
                (method, path, status)
            }
            Err(e) => {
                let reason = match e.status {
                    400 => "Bad Request",
                    404 => "Not Found",
                    405 => "Method Not Allowed",
                    409 => "Conflict",
                    499 => "Client Closed Request",
                    501 => "Not Implemented",
                    503 => "Service Unavailable",
                    _ => "Internal Server Error",
                };
                let _ = write_json(stream, e.status, reason, &[], &e.to_json().to_string());
                (method, path, e.status)
            }
        }
    }
}

/// Convenience: build the server context from an [`App`].
pub fn ctx_for(app: Arc<crate::app::App>) -> Arc<ApiCtx> {
    Arc::new(ApiCtx::new(app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use std::io::{Read, Write};

    struct Env {
        _dir: tempfile::TempDir,
        ctx: Arc<ApiCtx>,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        Env {
            ctx: Arc::new(ApiCtx::new(app)),
            _dir: dir,
        }
    }

    fn start(env: &Env, token: Option<String>, rate: u32) -> (SocketAddr, Arc<AtomicBool>) {
        let config = ApiServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            token,
            rate_limit_per_sec: rate,
        };
        let server = ApiServer::bind(config, env.ctx.clone()).unwrap();
        let addr = server.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        std::thread::spawn(move || {
            let _ = server.run(&flag);
        });
        // Give the accept loop a moment to start.
        std::thread::sleep(Duration::from_millis(50));
        (addr, shutdown)
    }

    /// One raw HTTP request; returns (status, headers, body).
    fn raw(
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, Vec<(String, String)>, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        req.push_str(body);
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        let headers = lines
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
            .collect();
        (status, headers, body.to_string())
    }

    fn authed(token: &str) -> Vec<(&'static str, String)> {
        vec![("Authorization", format!("Bearer {token}"))]
    }

    #[test]
    fn health_needs_no_token_and_status_reports_shape() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let (status, _, body) = raw(addr, "GET", "/v1/health", &[], "");
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"], "ok");
        let (status, _, _) = raw(addr, "GET", "/v1/status", &[], "");
        assert_eq!(status, 401, "status requires the token");
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/status", &auth_refs, "");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model_configured"], false);
        assert_eq!(v["enabled_jobs"], 0);
        assert_eq!(v["pending_approvals"], 0);
        assert_eq!(v["daemon"], "");
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn wrong_token_is_401_and_unknown_paths_are_404() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let bad = vec![("Authorization", "Bearer nope")];
        let (status, _, body) = raw(addr, "GET", "/v1/status", &bad, "");
        assert_eq!(status, 401);
        assert!(body.contains("unauthorized"), "{body}");
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/nope", &auth_refs, "");
        assert_eq!(status, 404);
        assert!(body.contains("not_found"), "{body}");
        let (status, _, _) = raw(addr, "GET", "/other", &auth_refs, "");
        assert_eq!(status, 404);
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn wrong_method_is_405_and_rate_limit_is_429() {
        let env = env();
        let (addr, shutdown) = start(&env, None, 2);
        let (status, _, body) = raw(addr, "POST", "/v1/health", &[], "");
        assert_eq!(status, 405);
        assert!(body.contains("method_not_allowed"), "{body}");
        // Rate limit: 2/sec; the POST above already used one slot, so
        // the next two health checks exhaust the window.
        let (s1, _, _) = raw(addr, "GET", "/v1/health", &[], "");
        assert_eq!(s1, 200);
        let (s2, headers, body) = raw(addr, "GET", "/v1/health", &[], "");
        assert_eq!(s2, 429);
        assert!(headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("retry-after") && v == "1"));
        assert!(body.contains("rate_limited"), "{body}");
        shutdown.store(true, Ordering::SeqCst);
    }
}
