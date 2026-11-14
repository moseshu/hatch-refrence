//! Integration tests for the vault helper's HTTP paths: helper-performed
//! Bearer requests, expiry / refresh state, and the OAuth authorization-code
//! flow — all against a local mock server. No real network, no real
//! credentials (every token is an obviously fake `test-token-...` value).

use hatch_vault::client::{StoreRequest, VaultClient};
use hatch_vault::oauth::{begin_flow, exchange_code, OAuthConfig};
use hatch_vault::record::now_unix;
use hatch_vault::open;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn start_client(home: &std::path::Path) -> VaultClient {
    VaultClient::start(home).expect("start vault helper")
}

// --------------------------------------------------------------- mock server


/// Local mock auth/refresh/protected server for the vault tests.
struct MockAuthServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockAuthServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((s, _)) => handle_auth_conn(s),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        MockAuthServer {
            base_url: format!("http://127.0.0.1:{port}"),
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for MockAuthServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn read_http(stream: &mut TcpStream) -> (String, String, HashMap<String, String>, Vec<u8>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    let mut len = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).unwrap();
    }
    (method, path, headers, body)
}

fn http_reply(stream: &mut TcpStream, status: u16, body: &str) {
    let text = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

fn handle_auth_conn(mut stream: TcpStream) {
    let (method, path, headers, body) = read_http(&mut stream);
    if method == "POST" && path == "/token" {
        let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        if v.get("code").and_then(Value::as_str) == Some("test-code-123") {
            return http_reply(
                &mut stream,
                200,
                r#"{"access_token":"test-token-oauth","expires_in":3600,"refresh_token":"test-refresh-oauth"}"#,
            );
        }
        return http_reply(&mut stream, 400, r#"{"error":"bad code"}"#);
    }
    if method == "POST" && path == "/refresh" {
        let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        if v.get("refresh_token").and_then(Value::as_str) == Some("test-refresh-oauth") {
            return http_reply(
                &mut stream,
                200,
                r#"{"access_token":"test-token-refreshed","expires_in":3600,"refresh_token":"test-refresh-oauth"}"#,
            );
        }
        return http_reply(&mut stream, 400, r#"{"error":"bad refresh token"}"#);
    }
    if method == "GET" && path == "/protected" {
        let auth = headers.get("authorization").cloned().unwrap_or_default();
        if auth == "Bearer test-token-oauth" || auth == "Bearer test-token-refreshed" {
            return http_reply(&mut stream, 200, r#"{"ok":true}"#);
        }
        return http_reply(&mut stream, 401, r#"{"error":"unauthorized"}"#);
    }
    http_reply(&mut stream, 404, r#"{"error":"not found"}"#);
}

#[test]
fn helper_request_adds_bearer_header() {
    let server = MockAuthServer::start();
    let home = temp_home();
    let client = start_client(home.path());
    client
        .store("api", StoreRequest { value: "test-token-oauth".into(), ..Default::default() })
        .unwrap();
    let out = client
        .request("api", "GET", &format!("{}/protected", server.base_url), None)
        .unwrap();
    assert_eq!(out.status, 200);
    assert!(out.body.contains("\"ok\":true"), "{}", out.body);
    assert!(!out.body.contains("test-token-oauth"));
}

#[test]
fn expiry_is_reported_and_refresh_works_against_mock() {
    let server = MockAuthServer::start();
    let home = temp_home();
    let client = start_client(home.path());
    // Expired token, refresh supported, mock refresh endpoint.
    client
        .store(
            "api",
            StoreRequest {
                value: "test-token-old".into(),
                expires_at: Some(now_unix() - 60),
                refresh_token: Some("test-refresh-oauth".into()),
                refresh_supported: true,
                refresh_url: Some(format!("{}/refresh", server.base_url)),
                ..Default::default()
            },
        )
        .unwrap();
    let items = client.list().unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].expired, "past expiry must be reported");
    assert!(items[0].has_refresh_token);

    client.refresh("api").unwrap();
    let items = client.list().unwrap();
    assert!(!items[0].expired, "refresh must clear the expiry");
    // The new value works against the mock protected endpoint.
    let out = client
        .request("api", "GET", &format!("{}/protected", server.base_url), None)
        .unwrap();
    assert_eq!(out.status, 200);
}

#[test]
fn refresh_without_support_reports_not_supported() {
    let home = temp_home();
    let client = start_client(home.path());
    client
        .store("plain", StoreRequest { value: "test-token-abc".into(), ..Default::default() })
        .unwrap();
    let e = client.refresh("plain").unwrap_err();
    assert!(e.to_string().contains("refresh_unsupported"), "{e}");
}

#[test]
fn legacy_string_store_migrates_to_records() {
    let home = temp_home();
    let vault = open(home.path()).unwrap();
    std::fs::write(&vault.path, r#"{"legacy": "test-token-legacy"}"#).unwrap();
    // The file API reads legacy values; the helper migrates on load.
    assert_eq!(vault.get("legacy").unwrap(), Some("test-token-legacy".into()));
    let client = start_client(home.path());
    let items = client.list().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "legacy");
    // Storing through the helper rewrites the file in record format.
    client
        .store("legacy", StoreRequest { value: "test-token-legacy".into(), ..Default::default() })
        .unwrap();
    drop(client);
    let text = std::fs::read_to_string(&vault.path).unwrap();
    assert!(text.contains("\"value\""), "file should now be record format: {text}");
    let rec = vault.get_record("legacy").unwrap().unwrap();
    assert_eq!(rec.value, "test-token-legacy");
}

#[test]
fn oauth_mock_flow_end_to_end() {
    let server = MockAuthServer::start();
    let home = temp_home();
    let client = start_client(home.path());
    let config = OAuthConfig {
        client_id: "test-client".into(),
        auth_url: format!("{}/authorize", server.base_url),
        token_url: format!("{}/token", server.base_url),
        scope: "read".into(),
        token_name: "oauth-mock".into(),
        connector: Some("mock".into()),
        revoke_url: None,
    };
    let (url, pending) = begin_flow(config.clone()).unwrap();
    assert!(url.contains("response_type=code"), "{url}");
    assert!(url.contains("test-client"), "{url}");
    let redirect = pending.redirect_uri().to_string();
    let state = url
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string();
    // Simulate the browser: the auth server "redirects" to loopback.
    let handle = std::thread::spawn(move || {
        pending.wait_for_code(Duration::from_secs(10))
    });
    std::thread::sleep(Duration::from_millis(100));
    let resp = ureq::get(&format!("{redirect}?code=test-code-123&state={state}"))
        .call()
        .expect("loopback redirect");
    assert_eq!(resp.status(), 200);
    let code = handle.join().expect("flow thread").expect("code");
    assert_eq!(code, "test-code-123");

    let result = exchange_code(&config, &redirect, &code, &client).unwrap();
    assert_eq!(result.token_name, "oauth-mock");
    assert!(result.expires_at.is_some());
    // Stored in the vault (via helper): list shows the name, and the value
    // works against the mock protected endpoint.
    assert_eq!(client.list_names().unwrap(), vec!["oauth-mock".to_string()]);
    let out = client
        .request("oauth-mock", "GET", &format!("{}/protected", server.base_url), None)
        .unwrap();
    assert_eq!(out.status, 200);
    // Sanity: the stored record carries the connector + refresh token.
    let vault = open(home.path()).unwrap();
    let rec = vault.get_record("oauth-mock").unwrap().unwrap();
    assert_eq!(rec.connector.as_deref(), Some("mock"));
    assert_eq!(rec.refresh_token.as_deref(), Some("test-refresh-oauth"));
    // And the provider identity a later refresh validates against.
    assert_eq!(
        rec.provider_url.as_deref(),
        Some(format!("{}/token", server.base_url).as_str())
    );
    assert_eq!(rec.refresh_url.as_deref(), rec.provider_url.as_deref());
    assert!(rec.refresh_supported);
    assert_eq!(rec.scope.as_deref(), Some("read"));
}
