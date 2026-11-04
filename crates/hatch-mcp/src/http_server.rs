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
//!   token is refused by the CLI before the server starts; without TLS
//!   it additionally needs an explicit `--allow-plain-http`.
//! * Authentication, per request: a per-client token from the registry
//!   (`<home>/mcp-clients.json`, see [`crate::client_tokens`]) wins
//!   first — its name becomes the approval origin suffix
//!   (`mcp:http:<name>`) and its scopes gate write tools; the legacy
//!   shared token (vault `mcp-http` / `HATCH_MCP_TOKEN`) still works
//!   with origin `mcp:http` and full scope. When neither is configured
//!   the server is unauthenticated (loopback only). The registry file
//!   is reloaded on every request, so revoking a token takes effect on
//!   the next request with no restart.
//! * TLS: when a certificate + key are configured the server terminates
//!   TLS in-process (rustls, ring); see [`crate::tls`].
//! * Fixed-window rate limit per client IP (default 20 requests /
//!   second) and a cap on concurrent sessions (default 32); exceeding
//!   either returns `429` with a `Retry-After: 1` header.
//! * Requests are logged as `method path status session=<first 8 chars
//!   of the session id>` only — never tokens, never bodies.

use crate::http_auth::{authenticate, Auth};
use crate::http_proto::{new_session_id, parse_request, write_response, HttpRequest, IoStream};
use crate::server::Server;
use crate::tls::load_server_config;
use hatch_core::{Error, Result};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::PathBuf;
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
    /// Legacy shared bearer token required on every request, when set.
    /// `None` means no shared token — only safe on loopback, enforced
    /// by the CLI. Per-client tokens (see `registry_path`) are checked
    /// first; this stays as the backward-compatible fallback.
    pub token: Option<String>,
    /// Per-client token registry file (`<home>/mcp-clients.json`).
    /// `None` = no per-client auth. When the file exists, a request
    /// must present a valid per-client token or the legacy token.
    pub registry_path: Option<PathBuf>,
    /// TLS certificate (PEM chain) for in-process termination. Both
    /// `tls_cert` and `tls_key` must be set together; `None` = plain
    /// HTTP (loopback, or non-loopback only with `--allow-plain-http`).
    pub tls_cert: Option<PathBuf>,
    /// TLS private key (PEM) matching `tls_cert`.
    pub tls_key: Option<PathBuf>,
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
            registry_path: None,
            tls_cert: None,
            tls_key: None,
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
    /// rustls server config when TLS is on (`None` = plain HTTP).
    tls: Option<Arc<rustls::ServerConfig>>,
    pub(crate) sessions: Mutex<HashMap<String, Session>>,
    /// Per-IP fixed-window counters: (window start, requests in window).
    limits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl HttpServer {
    /// Bind the listener. Does not serve yet; call [`HttpServer::run`].
    /// When `tls_cert`/`tls_key` are set, the TLS config is built here:
    /// a missing or unusable certificate/key is a startup error, never
    /// a half-secured server. Exactly one of the two set is also an
    /// error.
    pub fn bind(config: HttpServerConfig, server: Arc<Server>) -> std::io::Result<Arc<Self>> {
        let tls = match (&config.tls_cert, &config.tls_key) {
            (Some(cert), Some(key)) => Some(
                load_server_config(cert, key)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?,
            ),
            (None, None) => None,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "TLS 需要证书和私钥成对配置（--tls-cert 与 --tls-key 只能一起给）",
                ));
            }
        };
        let listener = TcpListener::bind(config.bind)?;
        listener.set_nonblocking(true)?;
        Ok(Arc::new(HttpServer {
            config,
            server,
            listener,
            tls,
            sessions: Mutex::new(HashMap::new()),
            limits: Mutex::new(HashMap::new()),
        }))
    }

    /// Whether this server terminates TLS in-process.
    pub fn tls_enabled(&self) -> bool {
        self.tls.is_some()
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
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
                    let stream: Box<dyn IoStream + Send> = match &self.tls {
                        Some(cfg) => match rustls::ServerConnection::new(Arc::clone(cfg)) {
                            Ok(conn) => Box::new(rustls::StreamOwned::new(conn, stream)),
                            Err(e) => {
                                eprintln!("[mcp-http] TLS 连接建立失败：{e}");
                                continue;
                            }
                        },
                        None => Box::new(stream),
                    };
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

    /// One JSON-RPC line through the shared [`Server`], with this
    /// request's origin and scope (per-client identity from auth).
    pub(crate) fn handle_line(&self, line: &str, auth: &Auth) -> Option<String> {
        self.server
            .handle_line_with(line, &auth.origin, auth.allow_write)
    }

    /// One connection: parse, route, reply, log. The stream is plain
    /// TCP or a rustls-wrapped connection — the same code serves both.
    /// Method/path/session/client only — never tokens or bodies.
    fn handle_conn(
        &self,
        mut stream: Box<dyn IoStream + Send>,
        peer_ip: IpAddr,
        shutdown: &AtomicBool,
    ) {
        let req = {
            let mut reader = BufReader::new(stream.as_mut());
            match parse_request(&mut reader) {
                Ok(r) => r,
                Err(_) => {
                    let _ = write_response(
                        stream.as_mut(),
                        400,
                        "Bad Request",
                        &[],
                        b"malformed request",
                    );
                    eprintln!("[mcp-http] {peer_ip} client=- status=400 session=");
                    return;
                }
            }
        };
        let (status, session, client) = self.dispatch(&mut stream, req, peer_ip, shutdown);
        eprintln!("[mcp-http] {peer_ip} client={client} status={status} session={session}");
    }

    /// Route one request. Returns the HTTP status, the short session id
    /// and the client name for the log line. Errors writing a reply are
    /// swallowed (the client is gone); the status is still reported.
    fn dispatch(
        &self,
        stream: &mut Box<dyn IoStream + Send>,
        req: HttpRequest,
        peer_ip: IpAddr,
        shutdown: &AtomicBool,
    ) -> (u16, String, String) {
        if req.path != "/mcp" {
            let _ = write_response(stream.as_mut(), 404, "Not Found", &[], b"not found");
            return (404, String::new(), "-".to_string());
        }
        if !self.allow(peer_ip) {
            let _ = write_response(
                stream.as_mut(),
                429,
                "Too Many Requests",
                &[("Retry-After", "1")],
                b"rate limit exceeded",
            );
            return (429, req.session_short(), "-".to_string());
        }
        let auth = match authenticate(&self.config, &req) {
            Ok(a) => a,
            Err(500) => {
                let _ = write_response(
                    stream.as_mut(),
                    500,
                    "Internal Server Error",
                    &[],
                    b"token registry unreadable",
                );
                return (500, req.session_short(), "-".to_string());
            }
            Err(status) => {
                let _ = write_response(
                    stream.as_mut(),
                    status,
                    "Unauthorized",
                    &[("WWW-Authenticate", "Bearer")],
                    b"missing or invalid bearer token",
                );
                return (status, req.session_short(), "-".to_string());
            }
        };
        let client = auth.client.clone();
        let (status, session) = match req.method.as_str() {
            "POST" => self.handle_post(stream.as_mut(), &req, &auth),
            "GET" => self.handle_get(stream.as_mut(), &req, shutdown),
            "DELETE" => self.handle_delete(stream.as_mut(), &req),
            _ => {
                let _ = write_response(
                    stream.as_mut(),
                    405,
                    "Method Not Allowed",
                    &[("Allow", "GET, POST, DELETE")],
                    b"method not allowed",
                );
                (405, req.session_short())
            }
        };
        (status, session, client)
    }
}
