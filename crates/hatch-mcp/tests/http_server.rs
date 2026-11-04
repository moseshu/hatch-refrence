//! MCP server over streamable HTTP: drive [`HttpServer`] with a real
//! `TcpStream` client on an ephemeral localhost port — no subprocess,
//! no model, no network service. The `handle_line` unit tests live in
//! `tests/server.rs`; the HTTP parsing/auth unit tests live in
//! `src/http_proto.rs`.

use hatch_connectors::mail::fixture::FileOutbox;
use hatch_mcp::{
    HttpServer, HttpServerConfig, Server, MCP_HTTP_ORIGIN, SERVER_HTTP_PROTOCOL_VERSION,
};
use hatch_policy::{ApprovalQueue, ApprovalStatus, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::{CreateEventExecutor, SendEmailExecutor};
use hatch_tools::{ExecutorRegistry, Workspace, WriteFileExecutor};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct Started {
    http: Arc<HttpServer>,
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    db: Arc<Db>,
    dir: tempfile::TempDir,
}

impl Drop for Started {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn start(mut config: HttpServerConfig) -> Started {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let ws = Workspace::new(dir.path()).unwrap();
    let reg = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: Some(ws),
            web_fetch: false,
            search_backend: None,
            max_command_timeout_secs: 5,
        },
    );
    let outbox = Arc::new(FileOutbox::new(dir.path(), "me@126.com").unwrap());
    let exec = ExecutorRegistry::new()
        .with(Arc::new(WriteFileExecutor))
        .with(Arc::new(SendEmailExecutor::new(outbox, "me@126.com")))
        .with(Arc::new(CreateEventExecutor));
    let server = Server::new(
        reg,
        exec,
        PolicyConfig::default(),
        Arc::clone(&db),
        chrono_tz::Asia::Shanghai,
        "me@126.com".into(),
        Duration::from_secs(10),
    )
    .with_origin(MCP_HTTP_ORIGIN)
    .with_protocol_version(SERVER_HTTP_PROTOCOL_VERSION);
    config.bind = "127.0.0.1:0".parse().unwrap();
    let http = HttpServer::bind(config, Arc::new(server)).unwrap();
    let port = http.local_addr().unwrap().port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let h = Arc::clone(&http);
    let s = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || h.run(&s).unwrap());
    Started {
        http,
        port,
        shutdown,
        handle: Some(handle),
        db,
        dir,
    }
}

/// One raw HTTP response.
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The JSON-RPC body: plain JSON, or the `data:` payload of an SSE
    /// event when the reply came back as a stream.
    fn json(&self) -> Value {
        let text = String::from_utf8_lossy(&self.body);
        let json_text = text
            .strip_prefix("data:")
            .map(str::trim)
            .unwrap_or_else(|| text.trim());
        serde_json::from_str(json_text).unwrap()
    }
}

fn connect(port: u16) -> TcpStream {
    let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

fn read_response(stream: &mut TcpStream) -> Resp {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status: u16 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).unwrap();
    Resp {
        status,
        headers,
        body,
    }
}

fn raw(port: u16, text: &str) -> Resp {
    let mut stream = connect(port);
    stream.write_all(text.as_bytes()).unwrap();
    read_response(&mut stream)
}

/// POST one JSON-RPC request; `sse` asks for a `text/event-stream`
/// reply instead of plain JSON.
fn rpc(
    port: u16,
    id: u64,
    method: &str,
    params: Value,
    session: Option<&str>,
    token: Option<&str>,
    sse: bool,
) -> Resp {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccept: {}\r\n",
        body.len(),
        if sse { "text/event-stream" } else { "application/json" },
    );
    if let Some(s) = session {
        req.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
    }
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    raw(port, &req)
}

fn init_params() -> Value {
    json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}})
}

fn initialize(port: u16, token: Option<&str>) -> (Resp, String) {
    let r = rpc(port, 1, "initialize", init_params(), None, token, false);
    assert_eq!(
        r.status,
        200,
        "initialize: {}",
        String::from_utf8_lossy(&r.body)
    );
    let sid = r
        .header("mcp-session-id")
        .expect("session id header")
        .to_string();
    (r, sid)
}

#[test]
fn http_flow_end_to_end() {
    let srv = start(HttpServerConfig::default());
    let port = srv.port;

    // initialize: negotiates 2025-03-26 and issues a session id.
    let (r, sid) = initialize(port, None);
    assert_eq!(r.json()["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(r.json()["result"]["serverInfo"]["name"], "hatch");
    assert_eq!(sid.len(), 32);

    // tools/list as an SSE reply: same tools as stdio, no mcp_* tools.
    let r = rpc(port, 2, "tools/list", json!({}), Some(&sid), None, true);
    assert_eq!(r.status, 200);
    assert!(r
        .header("content-type")
        .unwrap()
        .contains("text/event-stream"));
    let list_json = r.json();
    let names: Vec<&str> = list_json["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in ["memory_search", "fs_write", "approval_status"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert!(!names.iter().any(|n| n.starts_with("mcp_")), "{names:?}");

    // Read-only call executes directly.
    let r = rpc(
        port,
        3,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"query": "zzz-no-such-memory"}}),
        Some(&sid),
        None,
        false,
    );
    assert_eq!(r.json()["result"]["structuredContent"]["status"], "ok");

    // Write call queues an approval and does NOT execute.
    let r = rpc(
        port,
        4,
        "tools/call",
        json!({"name": "fs_write", "arguments": {"path": "note.txt", "content": "hello"}}),
        Some(&sid),
        None,
        false,
    );
    let sc = r.json()["result"]["structuredContent"].clone();
    assert_eq!(sc["status"], "approval_required", "{sc}");
    let approval_id = sc["approval_id"].as_str().unwrap().to_string();
    assert!(!srv.dir.path().join("note.txt").exists());
    let a = ApprovalQueue::new(&srv.db).get(&approval_id).unwrap();
    assert_eq!(a.status, ApprovalStatus::Pending);
    assert_eq!(a.origin, MCP_HTTP_ORIGIN);

    // approval_status polling over HTTP returns the queued approval.
    let r = rpc(
        port,
        5,
        "tools/call",
        json!({"name": "approval_status", "arguments": {"id": approval_id}}),
        Some(&sid),
        None,
        false,
    );
    assert!(
        r.json()["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pending"),
        "{}",
        r.json()
    );

    // GET /mcp opens an SSE stream for the session.
    let mut stream = connect(port);
    stream
        .write_all(
            format!("GET /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nMcp-Session-Id: {sid}\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("200"), "{line}");
    let mut saw_connected = false;
    for _ in 0..10 {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.trim() == ": connected" {
            saw_connected = true;
            break;
        }
    }
    assert!(saw_connected, "GET /mcp must send `: connected` first");
    drop(stream);

    // DELETE terminates the session; the id is dead afterwards.
    let r = raw(
        port,
        &format!("DELETE /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nMcp-Session-Id: {sid}\r\n\r\n"),
    );
    assert_eq!(r.status, 204);
    assert_eq!(srv.http.session_count(), 0);
    let r = rpc(port, 6, "tools/list", json!({}), Some(&sid), None, false);
    assert_eq!(r.status, 404);
}

#[test]
fn http_auth_rejects_missing_and_wrong_token() {
    let srv = start(HttpServerConfig {
        token: Some("test-token-only".to_string()),
        ..Default::default()
    });
    let port = srv.port;
    // No token at all.
    let r = rpc(port, 1, "initialize", init_params(), None, None, false);
    assert_eq!(r.status, 401);
    // Wrong token.
    let bad = Some("not-the-token");
    let r = rpc(port, 1, "initialize", init_params(), None, bad, false);
    assert_eq!(r.status, 401);
    // Right token works.
    let tok = Some("test-token-only");
    let (_, sid) = initialize(port, tok);
    let r = rpc(port, 2, "tools/list", json!({}), Some(&sid), tok, false);
    assert_eq!(r.status, 200);
}

#[test]
fn http_session_id_required_and_unknown_is_404() {
    let srv = start(HttpServerConfig::default());
    let port = srv.port;
    // No session header after initialize exists.
    let r = rpc(port, 1, "tools/list", json!({}), None, None, false);
    assert_eq!(r.status, 400);
    // Unknown session id.
    let r = rpc(
        port,
        1,
        "tools/list",
        json!({}),
        Some("00000000000000000000000000000000"),
        None,
        false,
    );
    assert_eq!(r.status, 404);
    // Unknown session on GET and DELETE too.
    let r = raw(
        port,
        "GET /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nMcp-Session-Id: 00000000000000000000000000000000\r\n\r\n",
    );
    assert_eq!(r.status, 404);
    let r = raw(
        port,
        "DELETE /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nMcp-Session-Id: 00000000000000000000000000000000\r\n\r\n",
    );
    assert_eq!(r.status, 404);
}

#[test]
fn http_rate_limit_returns_429() {
    let srv = start(HttpServerConfig {
        rate_limit_per_sec: 3,
        ..Default::default()
    });
    let port = srv.port;
    let (_, sid) = initialize(port, None);
    // First requests in the window succeed.
    for id in 2..=3 {
        let r = rpc(port, id, "tools/list", json!({}), Some(&sid), None, false);
        assert_eq!(r.status, 200, "request {id} should pass the limit");
    }
    // Burst past the limit: at least one 429 with Retry-After (a 1 s
    // window boundary may fall inside the burst, so allow the first
    // requests of the new window through and only require that the
    // burst as a whole is throttled).
    let mut saw_429 = false;
    for id in 4..=10 {
        let r = rpc(port, id, "tools/list", json!({}), Some(&sid), None, false);
        if r.status == 429 {
            saw_429 = true;
            assert_eq!(r.header("retry-after"), Some("1"));
        }
    }
    assert!(saw_429, "burst past 3 req/s must hit 429");
}
