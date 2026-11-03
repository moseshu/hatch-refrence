//! HTTP MCP client tests against a local mock server (TcpListener on
//! 127.0.0.1, no real network, fake tokens only). The mock answers the
//! same methods as the stdio fake server, over plain JSON or SSE, and
//! records the headers it sees so the auth-header path can be checked.

use hatch_mcp::{HttpClient, McpServerConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Json,
    Sse,
    Slow,
    Status500,
}

struct Mock {
    addr: std::net::SocketAddr,
    mode: Arc<Mutex<Mode>>,
    /// Lowercased header names + values seen on requests.
    seen_headers: Arc<Mutex<Vec<(String, String)>>>,
}

fn mock_reply(req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let result = |v: Value| json!({"jsonrpc": "2.0", "id": id, "result": v});
    let error = |code: i64, msg: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}});
    match method {
        "initialize" => result(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-http", "version": "0.1"},
        })),
        "ping" => result(json!({})),
        "tools/list" => result(json!({"tools": [
            {"name": "echo", "description": "echoes back arguments",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "add", "description": "adds a and b",
             "inputSchema": {"type": "object", "properties": {
                "a": {"type": "number"}, "b": {"type": "number"}},
                "required": ["a", "b"]}},
        ]})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match name {
                "echo" => result(
                    json!({"content": [{"type": "text", "text": serde_json::to_string(&args).unwrap()}]}),
                ),
                "add" => {
                    let sum = params["arguments"]["a"].as_f64().unwrap_or(0.0)
                        + params["arguments"]["b"].as_f64().unwrap_or(0.0);
                    let text = if sum.fract() == 0.0 {
                        format!("{}", sum as i64)
                    } else {
                        format!("{sum}")
                    };
                    result(json!({"content": [{"type": "text", "text": text}]}))
                }
                _ => error(-32602, &format!("unknown tool {name}")),
            }
        }
        "resources/list" => result(json!({"resources": []})),
        "resources/read" => error(-32602, "no such resource"),
        "prompts/list" => result(json!({"prompts": []})),
        _ => error(-32601, &format!("Method not found: {method}")),
    }
}

fn handle_conn(
    mut stream: TcpStream,
    mode: Arc<Mutex<Mode>>,
    seen: Arc<Mutex<Vec<(String, String)>>>,
) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return, // client closed the connection
            Ok(_) => {}
            Err(_) => return,
        }
        if line.trim().is_empty() {
            continue;
        }
        let mut content_length = 0usize;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end().to_string();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                if let Ok(mut s) = seen.lock() {
                    s.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
            }
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let m = *mode.lock().unwrap();
        if m == Mode::Slow {
            std::thread::sleep(Duration::from_secs(3));
        }
        let out = if m == Mode::Status500 {
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_string()
        } else {
            let resp = mock_reply(&req);
            let text = serde_json::to_string(&resp).unwrap();
            if m == Mode::Sse {
                let sse_body = format!(": ping\n\nevent: message\ndata: {text}\n\n");
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nContent-Length: {}\r\n\r\n{}",
                    sse_body.len(),
                    sse_body
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    text.len(),
                    text
                )
            }
        };
        if stream.write_all(out.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

impl Mock {
    fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mode = Arc::new(Mutex::new(Mode::Json));
        let seen_headers = Arc::new(Mutex::new(Vec::new()));
        let m = Arc::clone(&mode);
        let s = Arc::clone(&seen_headers);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let m = Arc::clone(&m);
                let s = Arc::clone(&s);
                std::thread::spawn(move || handle_conn(stream, m, s));
            }
        });
        Mock {
            addr,
            mode,
            seen_headers,
        }
    }

    fn set_mode(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn cfg(&self, timeout_secs: u64) -> McpServerConfig {
        McpServerConfig {
            name: "mock-http".into(),
            transport: "http".into(),
            url: format!("http://{}/mcp", self.addr),
            timeout_secs,
            enabled: true,
            ..Default::default()
        }
    }

    fn saw_header(&self, name: &str, value: &str) -> bool {
        self.seen_headers
            .lock()
            .unwrap()
            .iter()
            .any(|(k, v)| k == name && v == value)
    }
}

#[test]
fn http_json_list_and_call() {
    let mock = Mock::start();
    let mut c = HttpClient::connect(&mock.cfg(10), &HashMap::new(), None).unwrap();
    let tools = c.list_tools().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "add"]);
    let r = c.call_tool("echo", json!({"hello": "world"})).unwrap();
    assert_eq!(r["content"][0]["text"], "{\"hello\":\"world\"}");
    let r = c.call_tool("add", json!({"a": 1, "b": 2})).unwrap();
    assert_eq!(r["content"][0]["text"], "3");
    assert!(c.call_tool("nope", json!({})).is_err());
    assert!(c.list_resources().unwrap().is_empty());
    assert!(c.list_prompts().unwrap().is_empty());
}

#[test]
fn http_sse_list_and_call() {
    let mock = Mock::start();
    mock.set_mode(Mode::Sse);
    let mut c = HttpClient::connect(&mock.cfg(10), &HashMap::new(), None).unwrap();
    let tools = c.list_tools().unwrap();
    assert_eq!(tools.len(), 2);
    let r = c.call_tool("add", json!({"a": 2, "b": 3})).unwrap();
    assert_eq!(r["content"][0]["text"], "5");
}

#[test]
fn http_timeout_is_an_error_not_a_hang() {
    let mock = Mock::start();
    mock.set_mode(Mode::Slow);
    let r = HttpClient::connect(&mock.cfg(1), &HashMap::new(), None);
    assert!(r.is_err(), "a server that sleeps 3s must fail a 1s timeout");
}

#[test]
fn http_server_error_status_is_reported_without_body() {
    let mock = Mock::start();
    mock.set_mode(Mode::Status500);
    let r = HttpClient::connect(&mock.cfg(10), &HashMap::new(), None);
    match r {
        Ok(_) => panic!("HTTP 500 must fail connect"),
        Err(e) => assert!(e.to_string().contains("500"), "{e}"),
    }
}

#[test]
fn http_auth_header_and_custom_headers_are_sent() {
    let mock = Mock::start();
    let mut cfg = mock.cfg(10);
    cfg.headers = vec!["X_FAKE_KEY".into()];
    let mut headers = HashMap::new();
    headers.insert("X_FAKE_KEY".into(), "fake-header-value-1".into());
    // Fake token only — never a real credential.
    let mut c = HttpClient::connect(&cfg, &headers, Some("fake-test-token-123")).unwrap();
    c.ping().unwrap();
    assert!(mock.saw_header("authorization", "Bearer fake-test-token-123"));
    // The header name is the env-var name, sent verbatim.
    assert!(mock.saw_header("x_fake_key", "fake-header-value-1"));
    // The token value must only ever appear on the wire: the client holds
    // no log line containing it (verified by inspection — the client code
    // never formats a header value into an error or log message).
}

#[test]
fn http_missing_url_fails() {
    let cfg = McpServerConfig {
        name: "no-url".into(),
        transport: "http".into(),
        url: String::new(),
        ..Default::default()
    };
    assert!(HttpClient::connect(&cfg, &HashMap::new(), None).is_err());
}
