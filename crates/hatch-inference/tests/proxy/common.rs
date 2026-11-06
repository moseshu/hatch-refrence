//! Shared harness for the proxy integration tests: a scripted mock
//! upstream (plain HTTP on loopback — the real gateway is unreachable
//! from this VM, and no test ever touches it) and a proxy runner.

use hatch_inference::ledger::read_all;
use hatch_inference::{ledger_path, ProxyConfig, ProxyServer};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub auth: Option<String>,
    pub body: Vec<u8>,
}

pub struct MockResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl MockResponse {
    pub fn json(status: u16, body: &str) -> Self {
        MockResponse {
            status,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        }
    }
}

pub struct MockUpstream {
    pub addr: SocketAddr,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockUpstream {
    pub fn start(script: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let recv = received.clone();
        let stop = shutdown.clone();
        let handle = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut buf = [0u8; 65536];
                        let mut raw = Vec::new();
                        // Read until the full request (headers + body) arrived.
                        loop {
                            match stream.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    raw.extend_from_slice(&buf[..n]);
                                    if request_complete(&raw) {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let (method, path, auth, body) = parse_recorded(&raw);
                        recv.lock().unwrap().push(RecordedRequest {
                            method,
                            path,
                            auth,
                            body,
                        });
                        let resp = queue.lock().unwrap().pop_front().unwrap_or_else(|| {
                            MockResponse::json(
                                200,
                                "{\"status\":\"completed\",\"output\":[],\"usage\":{}}",
                            )
                        });
                        let head = format!(
                            "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            resp.status,
                            resp.content_type,
                            resp.body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&resp.body);
                        let _ = stream.flush();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });
        MockUpstream {
            addr,
            received,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .map(|r| RecordedRequest {
                method: r.method.clone(),
                path: r.path.clone(),
                auth: r.auth.clone(),
                body: r.body.clone(),
            })
            .collect()
    }

    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn request_complete(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some(head_end) = text.find("\r\n\r\n") else {
        return false;
    };
    let len: usize = text[..head_end]
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    raw.len() >= head_end + 4 + len
}

fn parse_recorded(raw: &[u8]) -> (String, String, Option<String>, Vec<u8>) {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut auth = None;
    let mut len = 0usize;
    for l in lines {
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.eq_ignore_ascii_case("authorization") {
                auth = Some(v.trim().to_string());
            }
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let head_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(raw.len());
    let body = raw[head_end..].iter().take(len).cloned().collect();
    (method, path, auth, body)
}

// ---------------------------------------------------------------------------
// Proxy harness
// ---------------------------------------------------------------------------

pub struct Proxy {
    pub addr: SocketAddr,
    pub ledger: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    dir: std::path::PathBuf,
}

impl Proxy {
    pub fn start(upstream: SocketAddr, max_retries: u32, rate_limit: u32) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "hatch-infer-proxy-test-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = ProxyConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            upstream_base_url: format!("http://{upstream}/v1"),
            api_key: Some("proxy-key-123".into()),
            max_retries,
            backoff_base_ms: 1,
            max_requests_per_min: rate_limit,
            ledger_path: ledger_path(&dir),
            price_input_per_million: None,
            price_output_per_million: None,
            currency: "CNY".into(),
        };
        let server = ProxyServer::bind(cfg).unwrap();
        let addr = server.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let handle = thread::spawn(move || {
            let _ = server.run(&stop);
        });
        Proxy {
            addr,
            ledger: ledger_path(&dir),
            shutdown,
            handle: Some(handle),
            dir,
        }
    }

    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
}

pub fn post_json(addr: SocketAddr, path: &str, auth: &str, body: &str) -> (u16, Vec<u8>) {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let url = format!("http://{addr}{path}");
    match agent
        .post(&url)
        .set("Authorization", auth)
        .set("Content-Type", "application/json")
        .send_string(body)
    {
        Ok(r) => {
            let status = r.status();
            (status, r.into_string().unwrap_or_default().into_bytes())
        }
        Err(ureq::Error::Status(code, r)) => {
            (code, r.into_string().unwrap_or_default().into_bytes())
        }
        Err(e) => panic!("client error: {e}"),
    }
}

/// Raw request, returning the raw response bytes (chunk framing included).
pub fn raw_request(addr: SocketAddr, text: &str) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(text.as_bytes()).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    out
}

/// De-chunk a chunked HTTP response body (after the blank line).
#[allow(dead_code)]
pub fn dechunk(raw: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let Some(start) = text.find("\r\n\r\n") else {
        return Vec::new();
    };
    let mut rest = &raw[start + 4..];
    let mut out = Vec::new();
    while let Some(end) = rest.iter().position(|&b| b == b'\r') {
        let len = usize::from_str_radix(&String::from_utf8_lossy(&rest[..end]), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = end + 2;
        out.extend_from_slice(&rest[data_start..data_start + len]);
        rest = &rest[data_start + len + 2..];
    }
    out
}

/// Read the ledger, waiting up to ~5 s for `n` entries: the proxy
/// records after the last chunk is sent, so a fast client can read
/// the file before the entry lands. The window is generous on
/// purpose — under a full parallel `cargo test` load the proxy
/// thread can be descheduled for well over the old 2 s.
pub fn wait_entries(ledger: &std::path::Path, n: usize) -> Vec<hatch_inference::UsageEntry> {
    for _ in 0..250 {
        let (entries, _) = read_all(ledger);
        if entries.len() >= n {
            return entries;
        }
        thread::sleep(Duration::from_millis(20));
    }
    read_all(ledger).0
}

pub const RESPONSES_BODY: &str =
    r#"{"model":"gpt-6-luna","input":[{"role":"user","content":"hi"}],"store":false}"#;
#[allow(dead_code)]
pub const RESPONSES_OK: &str = r#"{"model":"gpt-6-luna","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"你好"}]}],"usage":{"input_tokens":50,"output_tokens":20,"input_tokens_details":{"cached_tokens":32},"output_tokens_details":{"reasoning_tokens":12}}}"#;
