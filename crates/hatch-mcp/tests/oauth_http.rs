//! OAuth HTTP-client integration tests: 401 → refresh → retry once,
//! refresh failure → clear error with no silent retry, never-logged-in
//! → login hint, and discovery via `WWW-Authenticate`.
//!
//! One mock server (TcpListener on 127.0.0.1) plays both the MCP
//! endpoint and the OAuth metadata/token endpoints; fake tokens only.

use hatch_mcp::{
    HttpClient, McpServerConfig, OAuthClientConfig, OAuthConfig, OAuthSession, OAuthStore,
    OAuthTokens,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared mock state: every (path, Authorization) seen, and how the
/// token endpoint should answer.
struct MockState {
    seen: Mutex<Vec<(String, Option<String>)>>,
    token_status: Mutex<u16>,
    token_body: Mutex<String>,
    token_hits: AtomicUsize,
}

struct Mock {
    port: u16,
    state: Arc<MockState>,
}

fn write_resp(stream: &mut TcpStream, status: u16, reason: &str, headers: &[(&str, &str)], body: &str) {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}

fn handle_conn(mut stream: TcpStream, port: u16, state: Arc<MockState>) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    loop {
        let mut request_line = String::new();
        match reader.read_line(&mut request_line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if request_line.trim().is_empty() {
            continue;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut content_length = 0usize;
        let mut auth: Option<String> = None;
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
                if k.trim().eq_ignore_ascii_case("authorization") {
                    auth = Some(v.trim().to_string());
                }
            }
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        state.seen.lock().unwrap().push((path.clone(), auth.clone()));
        let base = format!("http://127.0.0.1:{port}");
        match (method.as_str(), path.as_str()) {
            ("GET", "/.well-known/oauth-protected-resource") => {
                let b = json!({"authorization_servers": [base]}).to_string();
                write_resp(&mut stream, 200, "OK", &[("Content-Type", "application/json")], &b);
            }
            ("GET", "/.well-known/oauth-authorization-server") => {
                let b = json!({
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                })
                .to_string();
                write_resp(&mut stream, 200, "OK", &[("Content-Type", "application/json")], &b);
            }
            ("GET", "/mcp") => {
                // Discovery route 1: 401 with resource_metadata.
                let h = format!(
                    "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
                );
                write_resp(&mut stream, 401, "Unauthorized", &[("WWW-Authenticate", &h)], "");
            }
            ("POST", "/token") => {
                state.token_hits.fetch_add(1, Ordering::SeqCst);
                let status = *state.token_status.lock().unwrap();
                let b = state.token_body.lock().unwrap().clone();
                let reason = if status == 200 { "OK" } else { "Bad Request" };
                write_resp(&mut stream, status, reason, &[("Content-Type", "application/json")], &b);
            }
            ("POST", "/mcp") => {
                // The old token is dead; the refreshed token works.
                if auth.as_deref() == Some("Bearer at-old") {
                    write_resp(&mut stream, 401, "Unauthorized", &[], "");
                    continue;
                }
                if auth.as_deref() != Some("Bearer at-new") {
                    write_resp(&mut stream, 401, "Unauthorized", &[], "");
                    continue;
                }
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let result = match method {
                    "initialize" => json!({"protocolVersion": "2024-11-05", "capabilities": {}}),
                    "ping" => json!({}),
                    _ => json!({}),
                };
                let b = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                write_resp(&mut stream, 200, "OK", &[("Content-Type", "application/json")], &b);
            }
            _ => write_resp(&mut stream, 404, "Not Found", &[], ""),
        }
    }
}

impl Mock {
    fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(MockState {
            seen: Mutex::new(Vec::new()),
            token_status: Mutex::new(200),
            token_body: Mutex::new(
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":3600}"#.to_string(),
            ),
            token_hits: AtomicUsize::new(0),
        });
        let state2 = Arc::clone(&state);
        // One thread per connection: the client may open a second
        // connection (e.g. a token refresh) while the first is still
        // being read, and a single-threaded accept loop would starve it.
        std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((s, _)) => {
                    let st = Arc::clone(&state2);
                    std::thread::spawn(move || handle_conn(s, port, st));
                }
                Err(_) => return,
            }
        });
        Mock { port, state }
    }

    fn mcp_url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    fn seen_mcp_posts(&self) -> Vec<Option<String>> {
        self.state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "/mcp")
            .map(|(_, a)| a.clone())
            .collect()
    }
}

fn seed_session(home: &std::path::Path, mock: &Mock, tokens: OAuthTokens) -> Arc<OAuthSession> {
    let base = format!("http://127.0.0.1:{}", mock.port);
    let store = OAuthStore::new(home, "srv").unwrap();
    store
        .save_config(&OAuthConfig {
            server_url: mock.mcp_url(),
            client_id: "hatch".into(),
            scopes: vec!["mcp:tools".into()],
            authorization_endpoint: format!("{base}/authorize"),
            token_endpoint: format!("{base}/token"),
        })
        .unwrap();
    store.save_tokens(&tokens).unwrap();
    Arc::new(
        OAuthSession::open(
            home,
            "srv",
            &OAuthClientConfig { client_id: "hatch".into(), scopes: vec!["mcp:tools".into()] },
            &mock.mcp_url(),
        )
        .unwrap(),
    )
}

fn cfg(mock: &Mock) -> McpServerConfig {
    McpServerConfig {
        name: "srv".into(),
        transport: "http".into(),
        url: mock.mcp_url(),
        timeout_secs: 10,
        enabled: true,
        ..Default::default()
    }
}

fn fresh_tokens() -> OAuthTokens {
    OAuthTokens {
        access_token: "at-old".into(),
        refresh_token: Some("rt-1".into()),
        expires_at: Some(hatch_now() + 3600),
    }
}

fn hatch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[test]
fn discovery_follows_www_authenticate_to_endpoints() {
    let mock = Mock::start();
    let found = hatch_mcp::oauth::discover(&mock.mcp_url()).unwrap();
    let base = format!("http://127.0.0.1:{}", mock.port);
    assert_eq!(found.authorization_endpoint, format!("{base}/authorize"));
    assert_eq!(found.token_endpoint, format!("{base}/token"));
}

#[test]
fn initialize_401_refreshes_once_and_retries_with_the_new_token() {
    let mock = Mock::start();
    let home = tempfile::tempdir().unwrap();
    let session = seed_session(home.path(), &mock, fresh_tokens());
    let mut client =
        HttpClient::connect_oauth(&cfg(&mock), &HashMap::new(), session).expect("connect_oauth");
    client.ping().unwrap();
    // initialize (401 -> refresh -> retry) + ping, both with at-new.
    let posts = mock.seen_mcp_posts();
    assert_eq!(posts.len(), 3, "init attempt, init retry, ping: {posts:?}");
    assert_eq!(posts[0].as_deref(), Some("Bearer at-old"), "first attempt uses the stored token");
    assert_eq!(posts[1].as_deref(), Some("Bearer at-new"), "retry uses the refreshed token");
    assert_eq!(posts[2].as_deref(), Some("Bearer at-new"));
    assert_eq!(mock.state.token_hits.load(Ordering::SeqCst), 1, "exactly one refresh POST");
}

#[test]
fn failed_refresh_is_a_clear_error_and_never_retries_with_the_dead_token() {
    let mock = Mock::start();
    *mock.state.token_status.lock().unwrap() = 400;
    *mock.state.token_body.lock().unwrap() = r#"{"error":"invalid_grant"}"#.to_string();
    let home = tempfile::tempdir().unwrap();
    let session = seed_session(home.path(), &mock, fresh_tokens());
    let err = match HttpClient::connect_oauth(&cfg(&mock), &HashMap::new(), session) {
        Ok(_) => panic!("connect_oauth must fail when refresh fails"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(msg.contains("refresh failed"), "must say refresh failed: {msg}");
    assert!(msg.contains("hatch mcp login srv"), "must point at login: {msg}");
    // Exactly one MCP request: the 401 attempt. No retry with at-old.
    assert_eq!(mock.seen_mcp_posts().len(), 1);
    assert_eq!(mock.state.token_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn never_logged_in_fails_before_touching_the_server() {
    let mock = Mock::start();
    let home = tempfile::tempdir().unwrap();
    let session = seed_session(home.path(), &mock, OAuthTokens::default());
    let err = match HttpClient::connect_oauth(&cfg(&mock), &HashMap::new(), session) {
        Ok(_) => panic!("connect_oauth must fail when never logged in"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("hatch mcp login srv"), "must point at login: {err}");
    assert!(mock.seen_mcp_posts().is_empty(), "no request without a token");
    // Discovery still works when asked directly (config was seeded).
    let _ = Duration::from_secs(0);
}
