//! The inference proxy server: loopback HTTP, one thread per
//! connection, no framework, no tokio (same shape as the MCP HTTP
//! server).
//!
//! Request flow: parse → `GET /health` → rate limit → forward to
//! the upstream with retries → stream the response back chunked
//! while scanning it for provider-reported usage → append one
//! ledger entry → log one line (method, path, status, model,
//! tokens, duration — never bodies or keys).
//!
//! Only `POST` is proxied. Everything else is 405, except
//! `GET /health`, which answers 200 without touching the upstream.

use crate::config::ProxyConfig;
use crate::forward::{forward_with_retry, ForwardError};
use crate::http::{
    end_chunks, parse_request, write_chunk, write_chunked_head, write_json, write_response,
    HttpRequest,
};
use crate::ledger::{UsageEntry, UsageLedger};
use crate::usage_extract::{model_from_body, SseUsageScanner};
use hatch_core::time::{now, to_rfc3339};
use hatch_llm::Usage;
use std::io::{BufReader, Read};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// HTTP reason phrases for the statuses the proxy can return.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

pub struct ProxyServer {
    config: ProxyConfig,
    agent: ureq::Agent,
    ledger: Arc<Mutex<UsageLedger>>,
    listener: TcpListener,
    /// Fixed-window rate limit: (window start, requests in window).
    limits: Mutex<(Instant, u32)>,
}

impl ProxyServer {
    /// Validate the config, open the ledger and bind the listener.
    /// Does not serve yet; call [`ProxyServer::run`].
    pub fn bind(config: ProxyConfig) -> std::io::Result<Arc<Self>> {
        config
            .validate()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let agent =
            hatch_llm::stream_agent(&config.upstream_base_url).map_err(std::io::Error::other)?;
        let ledger = UsageLedger::open(&config.ledger_path)?;
        let listener = TcpListener::bind(config.bind)?;
        listener.set_nonblocking(true)?;
        Ok(Arc::new(ProxyServer {
            config,
            agent,
            ledger: Arc::new(Mutex::new(ledger)),
            listener,
            limits: Mutex::new((Instant::now(), 0)),
        }))
    }

    /// The address actually bound (useful with port 0 in tests).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until `shutdown` is set. One thread per connection, via
    /// a scoped thread pool so connection threads can observe
    /// `shutdown`. On shutdown the accept loop stops and `run` waits
    /// for in-flight connections to finish.
    pub fn run(&self, shutdown: &AtomicBool) -> std::io::Result<()> {
        std::thread::scope(|scope| loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(120)));
                    scope.spawn(move || self.handle_conn(stream, addr.ip()));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if shutdown.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(e);
                }
            }
        })
    }

    /// Fixed-window rate limit: at most `max_requests_per_min`
    /// requests per minute, across all clients. `0` = unlimited.
    fn allow(&self) -> bool {
        let max = self.config.max_requests_per_min;
        if max == 0 {
            return true;
        }
        let mut limits = self.limits.lock().unwrap();
        let now = Instant::now();
        if now.duration_since(limits.0) >= Duration::from_secs(60) {
            *limits = (now, 1);
            return true;
        }
        if limits.1 < max {
            limits.1 += 1;
            return true;
        }
        false
    }

    fn handle_conn(&self, stream: TcpStream, _peer: IpAddr) {
        let mut reader = BufReader::new(&stream);
        let req = match parse_request(&mut reader) {
            Ok(r) => r,
            Err(_) => {
                let mut w = stream;
                let _ = write_response(&mut w, 400, "Bad Request", &[], b"malformed request");
                return;
            }
        };
        drop(reader);
        let mut stream = stream;
        self.dispatch(&mut stream, req);
    }

    /// Headers forwarded upstream: everything except hop-by-hop
    /// framing, `Host` (ureq sets it from the URL), `Accept-Encoding`
    /// (the proxy forces identity so usage extraction sees plain
    /// text) and `Authorization` (replaced below).
    fn forward_headers(&self, req: &HttpRequest) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = req
            .headers
            .iter()
            .filter(|(k, _)| {
                !matches!(
                    k.to_ascii_lowercase().as_str(),
                    "host"
                        | "authorization"
                        | "content-length"
                        | "connection"
                        | "accept-encoding"
                        | "transfer-encoding"
                )
            })
            .cloned()
            .collect();
        let auth = match &self.config.api_key {
            Some(key) => format!("Bearer {key}"),
            None => req.header("authorization").unwrap_or("").to_string(),
        };
        if !auth.is_empty() {
            out.push(("Authorization".to_string(), auth));
        }
        out
    }

    fn dispatch(&self, stream: &mut TcpStream, req: HttpRequest) {
        let started = Instant::now();
        if req.method == "GET" && req.path == "/health" {
            let _ = write_json(stream, 200, "OK", "{\"status\":\"ok\"}");
            return;
        }
        if req.method != "POST" {
            let _ = write_json(
                stream,
                405,
                "Method Not Allowed",
                "{\"error\":\"only POST is proxied\"}",
            );
            return;
        }
        if !self.allow() {
            let _ = write_response(
                stream,
                429,
                "Too Many Requests",
                &[("Content-Type", "application/json"), ("Retry-After", "60")],
                b"{\"error\":{\"code\":\"rate_limited\",\"message\":\"rate limit exceeded\"}}",
            );
            eprintln!("[inference-proxy] POST {} 429 rate_limited", req.path);
            return;
        }
        let model = model_from_body(&req.body);
        let url = self.config.upstream_url(&req.path, &req.query);
        let headers = self.forward_headers(&req);
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let max_retries = self.config.max_retries;
        let backoff = self.config.backoff_base_ms;
        match forward_with_retry(
            &self.agent,
            &url,
            &header_refs,
            &req.body,
            max_retries,
            backoff,
        ) {
            Ok((resp, retries)) => {
                self.stream_response(stream, req, resp, retries, model, started);
            }
            Err(ForwardError::Upstream { status, body }) => {
                let _ = write_response(
                    stream,
                    status,
                    reason(status),
                    &[("Content-Type", "application/json")],
                    &body,
                );
                self.record(&model, Usage::default(), started, 0, status, false);
                eprintln!(
                    "[inference-proxy] POST {} {status} upstream_error model={model}",
                    req.path
                );
            }
            Err(ForwardError::Transport(msg)) => {
                let body =
                    serde_json::json!({"error": {"code": "upstream_unavailable", "message": msg}})
                        .to_string();
                let _ = write_json(stream, 502, "Bad Gateway", &body);
                self.record(&model, Usage::default(), started, 0, 502, false);
                eprintln!(
                    "[inference-proxy] POST {} 502 upstream_unavailable model={model}",
                    req.path
                );
            }
        }
    }

    /// Stream one upstream response back chunked, scanning for usage
    /// as bytes arrive, then record the ledger entry and log one line.
    fn stream_response(
        &self,
        stream: &mut TcpStream,
        req: HttpRequest,
        mut resp: crate::forward::UpstreamResponse,
        retries: u32,
        model: String,
        started: Instant,
    ) {
        let status = resp.status;
        let is_json = resp
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.contains("application/json"));
        if write_chunked_head(stream, status, reason(status), &resp.headers).is_err() {
            return;
        }
        let mut scanner = SseUsageScanner::new();
        let mut json_body: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match resp.body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    if write_chunk(stream, chunk).is_err() {
                        return;
                    }
                    scanner.push(chunk);
                    if is_json && json_body.len() < 8 * 1024 * 1024 {
                        json_body.extend_from_slice(chunk);
                    }
                }
                Err(_) => break,
            }
        }
        let _ = end_chunks(stream);
        let usage = scanner.found().or_else(|| {
            if is_json {
                serde_json::from_slice::<serde_json::Value>(&json_body)
                    .ok()
                    .and_then(|v| crate::usage_extract::usage_from_json(&v))
            } else {
                None
            }
        });
        let usage = usage.unwrap_or_default();
        let ok = (200..300).contains(&status);
        self.record(&model, usage, started, retries, status, ok);
        eprintln!(
            "[inference-proxy] POST {} {status} model={model} prompt={} completion={} cached={} retries={retries} {}ms",
            req.path,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.cached_tokens,
            started.elapsed().as_millis()
        );
    }

    fn record(
        &self,
        model: &str,
        usage: Usage,
        started: Instant,
        retries: u32,
        status: u16,
        ok: bool,
    ) {
        let entry = UsageEntry {
            ts: to_rfc3339(&now()),
            model: model.to_string(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cached_tokens: usage.cached_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            duration_ms: started.elapsed().as_millis() as u64,
            retries,
            status,
            ok,
        };
        if let Ok(ledger) = self.ledger.lock() {
            let _ = ledger.record(&entry);
        }
    }
}
