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
        let _ = shutdown; // accepted by the accept loop; turns end on their own
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
            let status = chat::handle_message_stream(&self.ctx, stream, &req, &session);
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

    #[test]
    fn sessions_crud_roundtrip() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // Creating without a token is rejected.
        let (status, _, _) = raw(addr, "POST", "/v1/sessions", &[], r#"{"title":"t1"}"#);
        assert_eq!(status, 401);
        let (status, _, body) = raw(addr, "POST", "/v1/sessions", &auth, r#"{"title":"t1"}"#);
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["title"], "t1");
        let (status, _, body) = raw(addr, "GET", "/v1/sessions", &auth, "");
        assert_eq!(status, 200);
        assert!(body.contains(&id), "{body}");
        let (status, _, body) = raw(addr, "GET", &format!("/v1/sessions/{id}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["title"], "t1");
        assert_eq!(v["messages"], serde_json::json!([]));
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/sessions/{id}/fork"), &auth, "");
        assert_eq!(status, 201, "{body}");
        let fork = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert_ne!(fork["id"].as_str().unwrap(), id);
        let (status, _, _) =
            raw(addr, "POST", &format!("/v1/sessions/{id}/rewind"), &auth, "{}");
        assert_eq!(status, 400, "rewind needs to_seq");
        let (status, _, body) = raw(
            addr,
            "POST",
            &format!("/v1/sessions/{id}/rewind"),
            &auth,
            r#"{"to_seq":0}"#,
        );
        assert_eq!(status, 200, "{body}");
        let (status, _, _) = raw(addr, "GET", "/v1/sessions/ses_nope", &auth, "");
        assert_eq!(status, 404);
        let (status, _, _) = raw(addr, "POST", "/v1/sessions/ses_nope/fork", &auth, "");
        assert_eq!(status, 404);
        let (status, _, _) = raw(addr, "DELETE", &format!("/v1/sessions/{id}"), &auth, "");
        assert_eq!(status, 200);
        let (status, _, _) = raw(addr, "GET", &format!("/v1/sessions/{id}"), &auth, "");
        assert_eq!(status, 404);
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn memory_search_add_explain_people() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, _) = raw(addr, "GET", "/v1/memory/search", &auth, "");
        assert_eq!(status, 400, "search needs q");
        let (status, _, _) =
            raw(addr, "POST", "/v1/memory/claims", &auth, r#"{"subject":"x"}"#);
        assert_eq!(status, 400, "add needs text");
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/memory/claims",
            &auth,
            r#"{"subject":"api-test-subject","text":"the api test claim","tags":["t1"]}"#,
        );
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["duplicate"], false);
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/memory/claims",
            &auth,
            r#"{"subject":"api-test-subject","text":"the api test claim"}"#,
        );
        assert_eq!(status, 201, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["duplicate"],
            true
        );
        let (status, _, body) =
            raw(addr, "GET", "/v1/memory/search?q=api-test-subject", &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["id"], id);
        let (status, _, body) =
            raw(addr, "GET", &format!("/v1/memory/claims/{id}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["claim"]["id"],
            id
        );
        let (status, _, _) = raw(addr, "GET", "/v1/memory/claims/claim_nope", &auth, "");
        assert_eq!(status, 404);
        let (status, _, body) = raw(addr, "GET", "/v1/memory/people", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["people"],
            serde_json::json!([])
        );
        let (status, _, _) =
            raw(addr, "GET", "/v1/memory/people/nobody@example.com", &auth, "");
        assert_eq!(status, 404);
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn approvals_list_show_approve_deny() {
        use hatch_policy::{ActionClass, ApprovalQueue, NewApproval};
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/approvals", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["approvals"],
            serde_json::json!([])
        );
        let (status, _, _) = raw(addr, "GET", "/v1/approvals/apr_nope", &auth, "");
        assert_eq!(status, 404);
        let (status, _, _) =
            raw(addr, "POST", "/v1/approvals/apr_nope/approve", &auth, "{}");
        assert_eq!(status, 404);
        let (status, _, _) = raw(addr, "POST", "/v1/approvals/apr_nope/deny", &auth, "{}");
        assert_eq!(status, 404);
        // A real write_file approval, queued directly: approving it over
        // HTTP must execute into the temp workspace, and a second
        // approve must be a 409, never a second execution.
        let q = ApprovalQueue::new(&env.ctx.app.db);
        let submit = |key: &str, path: &str| {
            q.submit(
                &NewApproval {
                    idempotency_key: key.into(),
                    kind: "write_file".into(),
                    class: ActionClass::WriteFile,
                    title: format!("write {path}"),
                    preview: format!("write {path}"),
                    payload: serde_json::json!({
                        "root": env._dir.path(),
                        "path": path,
                        "content": "hello",
                        "base_hash": null,
                    }),
                    reasons: vec!["test".into()],
                    origin: "api:test".into(),
                    ttl: chrono::Duration::hours(24),
                },
                &hatch_core::time::now(),
            )
            .unwrap()
            .id()
            .to_string()
        };
        let id = submit("api-test-write", "api-test.txt");
        let (status, _, body) = raw(addr, "GET", &format!("/v1/approvals/{id}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["payload_intact"],
            true
        );
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/approvals/{id}/approve"), &auth, "{}");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["outcome"], "executed");
        assert!(env._dir.path().join("api-test.txt").exists());
        let (status, _, _) =
            raw(addr, "POST", &format!("/v1/approvals/{id}/approve"), &auth, "{}");
        assert_eq!(status, 409, "already decided: never execute twice");
        let id2 = submit("api-test-write-2", "api-test-2.txt");
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/approvals/{id2}/deny"), &auth, r#"{"note":"no"}"#);
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"],
            "rejected"
        );
        assert!(!env._dir.path().join("api-test-2.txt").exists());
        let (status, _, _) =
            raw(addr, "POST", &format!("/v1/approvals/{id2}/approve"), &auth, "{}");
        assert_eq!(status, 409, "denied approvals stay denied");
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn jobs_crud_roundtrip() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/jobs", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["jobs"],
            serde_json::json!([])
        );
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/jobs",
            &auth,
            r#"{"name":"j1","kind":"bogus","schedule":"0 9 * * *"}"#,
        );
        assert_eq!(status, 400, "unknown kind is rejected");
        let (status, _, _) =
            raw(addr, "POST", "/v1/jobs", &auth, r#"{"name":"j1"}"#);
        assert_eq!(status, 400, "missing fields are rejected");
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/jobs",
            &auth,
            r#"{"name":"j1","kind":"memory_flush","schedule":"0 9 * * *"}"#,
        );
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["name"], "j1");
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/jobs",
            &auth,
            r#"{"name":"j1","kind":"memory_flush","schedule":"0 9 * * *"}"#,
        );
        assert_eq!(status, 400, "duplicate name is rejected");
        let (status, _, _) = raw(addr, "GET", &format!("/v1/jobs/{id}"), &auth, "");
        assert_eq!(status, 200);
        let (status, _, body) = raw(addr, "GET", &format!("/v1/jobs/{id}/runs"), &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["runs"],
            serde_json::json!([])
        );
        // Run now: memory_flush is safe on an empty db; whatever its
        // outcome, the run must be recorded in history.
        let (status, _, body) = raw(addr, "POST", &format!("/v1/jobs/{id}/run"), &auth, "");
        assert_eq!(status, 200, "{body}");
        let (status, _, body) = raw(addr, "GET", &format!("/v1/jobs/{id}/runs"), &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body)
                .unwrap()["runs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let (status, _, _) = raw(
            addr,
            "PATCH",
            &format!("/v1/jobs/{id}"),
            &auth,
            r#"{"schedule":"garbage"}"#,
        );
        assert_eq!(status, 400, "bad schedule is rejected");
        let (status, _, body) = raw(
            addr,
            "PATCH",
            &format!("/v1/jobs/{id}"),
            &auth,
            r#"{"enabled":false,"schedule":"0 10 * * *"}"#,
        );
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["enabled"], false);
        // Schedule serializes with its kind prefix (ser_display).
        assert_eq!(v["schedule"], "cron 0 10 * * *");
        let (status, _, _) = raw(addr, "DELETE", &format!("/v1/jobs/{id}"), &auth, "");
        assert_eq!(status, 200);
        let (status, _, _) = raw(addr, "GET", &format!("/v1/jobs/{id}"), &auth, "");
        assert_eq!(status, 404);
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn artifacts_create_get_share_revoke() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, _) = raw(addr, "POST", "/v1/artifacts", &auth, "{}");
        assert_eq!(status, 400, "create needs title and content");
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/artifacts",
            &auth,
            r#"{"title":"t","content":"hi","kind":"bogus"}"#,
        );
        assert_eq!(status, 400, "unknown kind is rejected");
        // Secret scan: an API-key-shaped string is refused before writing.
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/artifacts",
            &auth,
            r#"{"title":"t","content":"leaked sk-abcdefghijklmnop1234 here"}"#,
        );
        assert_eq!(status, 400, "{body}");
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/artifacts",
            &auth,
            r#"{"title":"hello doc","content":"hello world"}"#,
        );
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["version"], 1);
        assert_eq!(v["shared"], false);
        let (status, _, body) = raw(addr, "GET", &format!("/v1/artifacts/{id}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["content"], "hello world");
        assert_eq!(v["versions"], serde_json::json!([1]));
        let (status, _, _) = raw(addr, "GET", "/v1/artifacts/art_nope", &auth, "");
        assert_eq!(status, 404);
        // Share submits an approval; sharing again reuses the same one.
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/artifacts/{id}/share"), &auth, "");
        assert_eq!(status, 202, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let approval_id = v["approval_id"].as_str().unwrap().to_string();
        let (status, _, _) =
            raw(addr, "GET", &format!("/v1/approvals/{approval_id}"), &auth, "");
        assert_eq!(status, 200, "share approval is in the queue");
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/artifacts/{id}/share"), &auth, "");
        assert_eq!(status, 202, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["approval_id"],
            approval_id,
            "same idempotency key, same approval"
        );
        let (status, _, _) =
            raw(addr, "POST", "/v1/artifacts/art_nope/share", &auth, "");
        assert_eq!(status, 404);
        // Revocation is 501 until share-token persistence lands.
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/artifacts/{id}/revoke"), &auth, "");
        assert_eq!(status, 501, "{body}");
        assert!(body.contains("not_implemented"), "{body}");
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn skills_connectors_lists() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/skills", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["skills"],
            serde_json::json!([])
        );
        let (status, _, body) = raw(addr, "GET", "/v1/connectors", &auth, "");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["connectors"], serde_json::json!([]));
        assert_eq!(v["warnings"], serde_json::json!([]));
        let (status, _, _) = raw(addr, "GET", "/v1/connectors/nope", &auth, "");
        assert_eq!(status, 404);
        // Drop a manifest (plus a broken one) into the temp home and
        // check the list picks them up, and the live status check runs.
        let dir = env._dir.path().join("connectors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("good.toml"),
            "name = \"good\"\ncommand = \"true\"\n[[ops]]\nname = \"status\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("bad.toml"), "name = \nbroken toml [[[\n").unwrap();
        let (status, _, body) = raw(addr, "GET", "/v1/connectors", &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let items = v["connectors"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "good");
        assert_eq!(items[0]["ops"][0]["name"], "status");
        assert_eq!(v["warnings"].as_array().unwrap().len(), 1);
        let (status, _, body) = raw(addr, "GET", "/v1/connectors/good", &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // `true` answers nothing, so the live check reports not-ok —
        // the point is the endpoint runs the check and reports it.
        assert_eq!(v["status"]["ok"], false);
        assert!(!v["status"]["detail"].as_str().unwrap().is_empty());
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn goals_feed_ideas_calendar_mail() {
        use hatch_connectors::{CalendarRepo, Event};
        use hatch_goals::{FeedStore, IdeaStore};
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/goals", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["goals"],
            serde_json::json!([])
        );
        let (status, _, _) = raw(addr, "POST", "/v1/goals", &auth, "{}");
        assert_eq!(status, 400, "create needs a title");
        let (status, _, body) =
            raw(addr, "POST", "/v1/goals", &auth, r#"{"title":"learn rust"}"#);
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let gid = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["status"], "active");
        let (status, _, body) = raw(addr, "GET", &format!("/v1/goals/{gid}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["title"], "learn rust");
        assert_eq!(v["files"], serde_json::json!([]));
        let (status, _, _) = raw(addr, "GET", "/v1/goals/goal_nope", &auth, "");
        assert_eq!(status, 404);
        // Seed one feed item, one idea and one event straight into the
        // stores, then check the read endpoints surface them.
        let now = hatch_core::time::now();
        FeedStore::new(&env.ctx.app.db)
            .save("feed title", "feed content", &["F1".into()], "test", false, &now)
            .unwrap();
        IdeaStore::new(&env.ctx.app.db)
            .add("test idea", "user", 1.0, &now)
            .unwrap();
        CalendarRepo::new(&env.ctx.app.db)
            .upsert(&Event {
                uid: "evt-1".into(),
                title: "standup".into(),
                starts_at: now + chrono::Duration::hours(1),
                ends_at: now + chrono::Duration::hours(2),
                location: String::new(),
                source: "test".into(),
            })
            .unwrap();
        let (status, _, body) = raw(addr, "GET", "/v1/feed", &auth, "");
        assert_eq!(status, 200);
        let items = serde_json::from_str::<serde_json::Value>(&body).unwrap()["feed"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "feed title");
        let (status, _, body) = raw(addr, "GET", "/v1/ideas", &auth, "");
        assert_eq!(status, 200);
        let items = serde_json::from_str::<serde_json::Value>(&body).unwrap()["ideas"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["text"], "test idea");
        let (status, _, body) = raw(addr, "GET", "/v1/calendar/events", &auth, "");
        assert_eq!(status, 200);
        let items = serde_json::from_str::<serde_json::Value>(&body).unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "standup");
        let (status, _, body) =
            raw(addr, "GET", "/v1/calendar/events?days=abc", &auth, "");
        assert_eq!(status, 200, "bad days falls back to the default");
        let _ = body;
        let (status, _, body) = raw(addr, "GET", "/v1/mail/triage", &auth, "");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["receipt"], serde_json::Value::Null);
        assert_eq!(v["drafts"], serde_json::json!([]));
        shutdown.store(true, Ordering::SeqCst);
    }
}
