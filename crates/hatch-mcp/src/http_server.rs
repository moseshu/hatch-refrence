//! MCP server over streamable HTTP (MCP spec 2025-03-26).
//!
//! A minimal HTTP/1.1 server on std `TcpListener` — no framework, no
//! tokio. One thread per connection. It reuses the same [`Server`] as
//! the stdio transport (built with origin `mcp:http` and protocol
//! `2025-03-26` by the CLI), so every `tools/call` goes through the
//! identical dispatch → gatekeeper → approval-queue path; nothing here
//! can bypass approval. `approval_status` polling works over HTTP
//! exactly as over stdio, scoped to the `mcp:http` origin.
//!
//! Endpoints (all under `/mcp`; handlers live in
//! [`crate::http_routes`]):
//!
//! * `POST /mcp` — one JSON-RPC request (or notification) per call.
//!   `initialize` creates a session and returns its id in the
//!   `Mcp-Session-Id` response header. Later requests must carry that
//!   header: missing → 400, unknown → 404. Notifications (no `id`)
//!   get `202` with an empty body. The reply is plain JSON by default,
//!   or an SSE stream (`data: <json>`) when the client sends
//!   `Accept: text/event-stream`. Batch requests (a JSON array) are
//!   rejected with `-32600` — the spec forbids them on this transport.
//! * `GET /mcp` — opens an SSE stream for server-initiated messages
//!   for an existing session. Today the server sends no notifications,
//!   so the stream only carries `: connected` on open and a `: ping`
//!   comment every 15 s until the client disconnects, the session is
//!   deleted, or the server shuts down. Missing session → 400, unknown
//!   → 404.
//! * `DELETE /mcp` — terminates a session; `204` on success, 404 for
//!   an unknown session.
//!
//! Security (see also `docs/SECURITY.md` §9d):
//!
//! * Binds `127.0.0.1` by default. A non-loopback bind without a bearer
//!   token is refused by the CLI before the server starts.
//! * Bearer token, when configured, is required on every request
//!   (constant-time comparison). It comes from the vault token
//!   `mcp-http` (preferred) or the `HATCH_MCP_TOKEN` environment
//!   variable — never from the command line, never logged, never
//!   written to a file.
//! * Fixed-window rate limit per client IP (default 20 requests /
//!   second) and a cap on concurrent sessions (default 32); exceeding
//!   either returns `429` with a `Retry-After: 1` header.
//! * Requests are logged as `method path status session=<first 8 chars
//!   of the session id>` only — never tokens, never bodies.

use crate::http_proto::{bearer_ok, new_session_id, parse_request, write_response};
use crate::server::Server;
use hatch_core::{Error, Result};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A session idle this long is pruned when the next one is created.
const SESSION_IDLE_SECS: u64 = 3600;

/// Configuration for [`HttpServer`].
#[derive(Clone, Debug)]
pub struct HttpServerConfig {
    /// Address to listen on. Default `127.0.0.1:8080`.
    pub bind: SocketAddr,
    /// Bearer token required on every request, when set. `None` means
    /// no authentication — only safe on loopback, enforced by the CLI.
    pub token: Option<String>,
    /// Fixed-window rate limit: requests per second per client IP.
    /// Default 20.
    pub rate_limit_per_sec: u32,
    /// Maximum concurrent sessions. Default 32.
    pub max_sessions: usize,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        HttpServerConfig {
            bind: "127.0.0.1:8080".parse().unwrap(),
            token: None,
            rate_limit_per_sec: 20,
            max_sessions: 32,
        }
    }
}

pub(crate) struct Session {
    last_seen: Instant,
}

/// The MCP server on streamable HTTP. Fields are `pub(crate)` so the
/// route handlers in [`crate::http_routes`] (a separate `impl` block)
/// can use them.
pub struct HttpServer {
    pub(crate) config: HttpServerConfig,
    pub(crate) server: Arc<Server>,
    listener: TcpListener,
    pub(crate) sessions: Mutex<HashMap<String, Session>>,
    /// Per-IP fixed-window counters: (window start, requests in window).
    limits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl HttpServer {
    /// Bind the listener. Does not serve yet; call [`HttpServer::run`].
    pub fn bind(config: HttpServerConfig, server: Arc<Server>) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind(config.bind)?;
        listener.set_nonblocking(true)?;
        Ok(Arc::new(HttpServer {
            config,
            server,
            listener,
            sessions: Mutex::new(HashMap::new()),
            limits: Mutex::new(HashMap::new()),
        }))
    }

    /// The address actually bound (useful with port 0 in tests).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Number of live sessions (sessions are pruned lazily).
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// Serve until `shutdown` is set. One thread per connection, via a
    /// scoped thread pool so connection threads can observe `shutdown`
    /// (notably the `GET /mcp` SSE streams, which close within a
    /// second of shutdown). On shutdown the accept loop stops and
    /// `run` waits for in-flight connections to finish.
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
                Err(e) => return Err(Error::Connector(format!("mcp http accept failed: {e}"))),
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

    /// Create a session, pruning ones idle over an hour first.
    /// `None` = the concurrent-session cap is reached.
    pub(crate) fn create_session(&self) -> Option<String> {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, s| s.last_seen.elapsed() < Duration::from_secs(SESSION_IDLE_SECS));
        if sessions.len() >= self.config.max_sessions {
            return None;
        }
        let id = new_session_id();
        sessions.insert(
            id.clone(),
            Session {
                last_seen: Instant::now(),
            },
        );
        Some(id)
    }

    /// Look a session up and refresh its idle timer. `false` = unknown.
    pub(crate) fn touch_session(&self, id: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get_mut(id) {
            Some(s) => {
                s.last_seen = Instant::now();
                true
            }
            None => false,
        }
    }

    pub(crate) fn remove_session(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().remove(id).is_some()
    }

    /// One JSON-RPC line through the shared [`Server`].
    pub(crate) fn handle_line(&self, line: &str) -> Option<String> {
        self.server.handle_line(line)
    }

    /// One connection: parse, route, reply, log. Method/path/session
    /// only — never tokens or bodies.
    fn handle_conn(&self, stream: TcpStream, peer_ip: IpAddr, shutdown: &AtomicBool) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
        let mut reader = match stream.try_clone() {
            Ok(c) => BufReader::new(c),
            Err(_) => return,
        };
        let mut stream = stream;
        let (status, session) = self.dispatch(&mut reader, &mut stream, peer_ip, shutdown);
        eprintln!("[mcp-http] {peer_ip} status={status} session={session}");
    }

    /// Route one request. Returns the HTTP status and the short session
    /// id for the log line. Errors writing a reply are swallowed (the
    /// client is gone); the status is still reported.
    fn dispatch(
        &self,
        reader: &mut BufReader<TcpStream>,
        stream: &mut TcpStream,
        peer_ip: IpAddr,
        shutdown: &AtomicBool,
    ) -> (u16, String) {
        let req = match parse_request(reader) {
            Ok(r) => r,
            Err(_) => {
                let _ = write_response(stream, 400, "Bad Request", &[], b"malformed request");
                return (400, String::new());
            }
        };
        if req.path != "/mcp" {
            let _ = write_response(stream, 404, "Not Found", &[], b"not found");
            return (404, String::new());
        }
        if !self.allow(peer_ip) {
            let _ = write_response(
                stream,
                429,
                "Too Many Requests",
                &[("Retry-After", "1")],
                b"rate limit exceeded",
            );
            return (429, req.session_short());
        }
        if let Some(token) = &self.config.token {
            if !bearer_ok(&req, token) {
                let _ = write_response(
                    stream,
                    401,
                    "Unauthorized",
                    &[("WWW-Authenticate", "Bearer")],
                    b"missing or invalid bearer token",
                );
                return (401, req.session_short());
            }
        }
        match req.method.as_str() {
            "POST" => self.handle_post(stream, &req),
            "GET" => self.handle_get(stream, &req, shutdown),
            "DELETE" => self.handle_delete(stream, &req),
            _ => {
                let _ = write_response(
                    stream,
                    405,
                    "Method Not Allowed",
                    &[("Allow", "GET, POST, DELETE")],
                    b"method not allowed",
                );
                (405, req.session_short())
            }
        }
    }
}
