//! The product API on plain HTTP/1.1 (std `TcpListener`, no framework).
//!
//! One thread per connection, mirroring the MCP streamable-HTTP server:
//! fixed-window rate limit per client IP, per-client bearer auth (see
//! [`super::client_auth`]: registry token, then the legacy shared
//! token, then anonymous loopback access), and method/path/status-only
//! logging — never tokens, never bodies. All replies are JSON
//! (`{"error": ...}` on failure) except the SSE chat stream.
//!
//! v1 is single-user: an `X-User-Id` header, when present, is accepted
//! and ignored. Mapping tokens to users is separate multi-tenant work
//! and is not pretended here.

use super::chat;
use super::client_auth;
use super::routes;
use super::{ApiCtx, ApiError};
use hatch_core::{Error, Result};
use hatch_mcp::http_proto::{parse_request, write_json};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Configuration for [`ApiServer`].
#[derive(Clone, Debug)]
pub struct ApiServerConfig {
    /// Address to listen on. Default `127.0.0.1:8787`.
    pub bind: SocketAddr,
    /// Legacy shared bearer token, required on every request except
    /// `/v1/health` and `/v1/shared/<token>`, when set. `None` means
    /// no shared token — only safe on loopback, enforced by the CLI
    /// before the server starts.
    pub token: Option<String>,
    /// Per-client token registry (`<home>/daemon-clients.json`).
    /// Reloaded from disk on every request, so revoke takes effect
    /// immediately. `None` disables per-client auth.
    pub registry_path: Option<PathBuf>,
    /// Fixed-window rate limit: requests per second per client IP.
    /// Default 20.
    pub rate_limit_per_sec: u32,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        ApiServerConfig {
            bind: "127.0.0.1:8787".parse().unwrap(),
            token: None,
            registry_path: None,
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
        let (method, path, status, client) =
            self.dispatch(&mut reader, &mut stream, peer_ip, shutdown);
        eprintln!("[api] {method} {path} {status} {client}");
    }

    /// Route one request. Returns method, path, status and the
    /// authenticated client name ("-" when unauthenticated) for the log
    /// line. Errors writing a reply are swallowed (the client is gone).
    fn dispatch(
        &self,
        reader: &mut BufReader<TcpStream>,
        stream: &mut TcpStream,
        peer_ip: IpAddr,
        shutdown: &AtomicBool,
    ) -> (&'static str, String, u16, String) {
        let _ = shutdown; // accepted by the accept loop; turns end on their own
        let req = match parse_request(reader) {
            Ok(r) => r,
            Err(_) => {
                let e = ApiError::bad_request("malformed request");
                let _ = write_json(stream, 400, "Bad Request", &[], &e.to_json().to_string());
                return ("?", String::new(), 400, "-".to_string());
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
        // Only `/v1/...` is served; anything else 404s with the
        // unified error body. (No static frontend: the frontend
        // lives in a separate repo and talks to this API.)
        if !path.starts_with("/v1/") {
            let e = ApiError::not_found("接口");
            let _ = write_json(stream, 404, "Not Found", &[], &e.to_json().to_string());
            return (method, path, 404, "-".to_string());
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
            return (method, path, 429, "-".to_string());
        }
        // `/v1/health` is the unauthenticated liveness probe, and
        // `/v1/shared/<token>` is the share link: the token in the
        // path is the credential, so it skips bearer auth too (the
        // handler still answers 404 for unknown/revoked tokens).
        // Everything else authenticates per request: per-client
        // registry token first, then the legacy shared token, then
        // anonymous loopback access when neither is configured.
        let auth = if path != "/v1/health" && !path.starts_with("/v1/shared/") {
            match client_auth::authenticate(
                self.config.token.as_deref(),
                self.config.registry_path.as_deref(),
                &req,
            ) {
                Ok(auth) => Some(auth),
                Err(status) => {
                    let e = if status == 500 {
                        ApiError::internal("token 注册表不可读")
                    } else {
                        ApiError::new(401, "unauthorized", "缺少或无效的 bearer token。")
                    };
                    let _ = write_json(
                        stream,
                        status,
                        if status == 500 {
                            "Internal Server Error"
                        } else {
                            "Unauthorized"
                        },
                        &[("WWW-Authenticate", "Bearer")],
                        &e.to_json().to_string(),
                    );
                    return (method, path, status, "-".to_string());
                }
            }
        } else {
            None
        };
        // Client name for the log line ("-" when unauthenticated).
        let client = auth
            .as_ref()
            .map(|a| a.client.clone())
            .unwrap_or_else(|| "-".to_string());
        // Scope: a read-only client token may only call GET endpoints.
        // The check runs before the SSE stream and the JSON dispatcher,
        // so a denied request never executes anything and never queues
        // an approval.
        if let Some(auth) = &auth {
            if !auth.allow_write && client_auth::requires_write(method) {
                let e = ApiError::new(
                    403,
                    "insufficient_scope",
                    "该 token 只有 read scope，不能调用写类端点。",
                );
                let _ = write_json(stream, 403, "Forbidden", &[], &e.to_json().to_string());
                return (method, path, 403, client);
            }
        }
        // SSE chat owns the stream for the whole turn.
        if let Some(session) = routes::chat_stream_session(&req) {
            let status = chat::handle_message_stream(&self.ctx, stream, &req, &session);
            return (method, path, status, client);
        }
        match routes::dispatch_json(&self.ctx, &req) {
            Ok((status, body)) => {
                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    _ => "OK",
                };
                let _ = write_json(stream, status, reason, &[], &body.to_string());
                (method, path, status, client)
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
                (method, path, e.status, client)
            }
        }
    }
}

/// Convenience: build the server context from an [`App`].
pub fn ctx_for(app: Arc<crate::app::App>) -> Arc<ApiCtx> {
    Arc::new(ApiCtx::new(app))
}
