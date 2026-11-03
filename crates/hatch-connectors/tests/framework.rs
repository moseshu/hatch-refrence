//! Integration tests for the connector framework: both reference
//! connectors end-to-end through the runner, status guidance, timeout kill,
//! and manifest loading. No real network, no real credentials — the mock
//! HTTP server is a local `TcpListener` and all tokens are obviously fake.

use hatch_connectors::framework::manifest::{ConnectorManifest, OpDef};
use hatch_connectors::framework::registry::load_dir;
use hatch_connectors::framework::runner;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn bin_path(env_name: &str, crate_name: &str, bin: &str) -> PathBuf {
    if let Ok(p) = std::env::var(env_name) {
        return p.into();
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let plain = manifest.join(format!("../../target/debug/{bin}"));
    if plain.is_file() {
        return plain;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = std::process::Command::new(&cargo)
        .args(["build", "-p", crate_name, "--bin", bin])
        .current_dir(manifest.parent().unwrap().parent().unwrap())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok && plain.is_file(), "could not build {bin}");
    plain
}

fn notes_bin() -> PathBuf {
    bin_path("CARGO_BIN_EXE_hatch-notes-connector", "hatch-connectors", "hatch-notes-connector")
}

fn mockhttp_bin() -> PathBuf {
    bin_path(
        "CARGO_BIN_EXE_hatch-mockhttp-connector",
        "hatch-connectors",
        "hatch-mockhttp-connector",
    )
}

fn notes_manifest() -> ConnectorManifest {
    ConnectorManifest {
        name: "notes".into(),
        command: notes_bin().to_string_lossy().into_owned(),
        args: vec![],
        env: vec!["HATCH_NOTES_DIR".into()],
        token_ref: None,
        token_env: "CONNECTOR_TOKEN".into(),
        timeout_secs: 10,
        ops: vec![
            OpDef { name: "status".into(), description: "status".into(), read_only: true, args_schema: json!({}) },
            OpDef { name: "list_notes".into(), description: "list".into(), read_only: true, args_schema: json!({}) },
            OpDef { name: "read_note".into(), description: "read".into(), read_only: true, args_schema: json!({}) },
            OpDef { name: "write_note".into(), description: "write".into(), read_only: false, args_schema: json!({}) },
            OpDef { name: "delete_note".into(), description: "delete".into(), read_only: false, args_schema: json!({}) },
        ],
        connect_guidance: "Set HATCH_NOTES_DIR to a notes directory.".into(),
    }
}

fn env_notes(dir: &std::path::Path) -> HashMap<String, String> {
    HashMap::from([(
        "HATCH_NOTES_DIR".to_string(),
        dir.to_string_lossy().into_owned(),
    )])
}

#[test]
fn notes_connector_end_to_end_via_runner() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let m = notes_manifest();
    let env = env_notes(&notes);

    let s = runner::status(&m, &env);
    assert!(s.ok, "status should pass: {}", s.detail);

    let r = runner::call(&m, "write_note", &json!({"name": "hello", "content": "world"}), &env)
        .unwrap();
    assert_eq!(r["written"], true);

    let r = runner::call(&m, "list_notes", &json!({}), &env).unwrap();
    assert_eq!(r["notes"], json!(["hello"]));

    let r = runner::call(&m, "read_note", &json!({"name": "hello"}), &env).unwrap();
    assert_eq!(r["content"], "world");

    let e = runner::call(&m, "read_note", &json!({"name": "missing"}), &env).unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");

    let r = runner::call(&m, "delete_note", &json!({"name": "hello"}), &env).unwrap();
    assert_eq!(r["deleted"], true);
    let r = runner::call(&m, "list_notes", &json!({}), &env).unwrap();
    assert_eq!(r["notes"], json!([]));
}

#[test]
fn notes_connector_rejects_bad_names_and_unknown_ops() {
    let dir = tempfile::tempdir().unwrap();
    let m = notes_manifest();
    let env = env_notes(dir.path());
    let e = runner::call(&m, "write_note", &json!({"name": "../evil", "content": "x"}), &env)
        .unwrap_err();
    assert!(e.to_string().contains("invalid_args"), "{e}");
    let e = runner::call(&m, "explode", &json!({}), &env).unwrap_err();
    assert!(e.to_string().contains("unknown_op"), "{e}");
}

#[test]
fn status_reports_guidance_when_unconfigured() {
    let m = notes_manifest();
    let s = runner::status(&m, &HashMap::new());
    assert!(!s.ok);
    assert!(s.detail.contains("not_configured"), "{}", s.detail);
    assert!(s.guidance.contains("HATCH_NOTES_DIR"), "{}", s.guidance);
}

#[test]
fn timeout_kills_a_hung_connector() {
    let m = ConnectorManifest {
        name: "hung".into(),
        command: "sleep".into(),
        args: vec!["30".into()],
        timeout_secs: 1,
        ops: vec![OpDef { name: "status".into(), ..Default::default() }],
        ..Default::default()
    };
    let start = std::time::Instant::now();
    let e = runner::call(&m, "status", &json!({}), &HashMap::new()).unwrap_err();
    assert!(e.to_string().contains("timed out"), "{e}");
    assert!(start.elapsed() < Duration::from_secs(10), "must fail fast, not hang");
}

// --------------------------------------------------------------- mock HTTP

/// A tiny local mock HTTP server. `/status` is open; `/items/*` requires
/// `Authorization: Bearer test-token-mock`.
struct MockServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((s, _)) => handle_conn(s),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        MockServer { base_url: format!("http://127.0.0.1:{port}"), stop, handle: Some(handle) }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> (String, String, HashMap<String, String>, Vec<u8>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).unwrap();
    }
    (method, path, headers, body)
}

fn reply(stream: &mut TcpStream, status: u16, body: &str) {
    let text = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

fn handle_conn(mut stream: TcpStream) {
    let (method, path, headers, body) = read_request(&mut stream);
    if path == "/status" {
        return reply(&mut stream, 200, r#"{"message":"mock service ok"}"#);
    }
    let authed = headers
        .get("authorization")
        .map(|v| v == "Bearer test-token-mock")
        .unwrap_or(false);
    if !authed {
        return reply(&mut stream, 401, r#"{"error":"unauthorized"}"#);
    }
    if method == "GET" && path == "/items/42" {
        return reply(&mut stream, 200, r#"{"id":"42","name":"thing"}"#);
    }
    if method == "GET" && path.starts_with("/items/") {
        return reply(&mut stream, 404, r#"{"error":"not found"}"#);
    }
    if method == "POST" && path == "/items" {
        let name: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        let name = name.get("name").and_then(Value::as_str).unwrap_or("?");
        return reply(
            &mut stream,
            200,
            &serde_json::to_string(&json!({"id": "1", "name": name})).unwrap(),
        );
    }
    reply(&mut stream, 404, r#"{"error":"not found"}"#);
}

fn mock_manifest(server: &MockServer) -> (ConnectorManifest, HashMap<String, String>) {
    let m = ConnectorManifest {
        name: "mockhttp".into(),
        command: mockhttp_bin().to_string_lossy().into_owned(),
        args: vec![],
        env: vec!["HATCH_MOCKHTTP_BASE_URL".into()],
        token_ref: Some("mock-token".into()),
        token_env: "CONNECTOR_TOKEN".into(),
        timeout_secs: 10,
        ops: vec![
            OpDef { name: "status".into(), description: "s".into(), read_only: true, args_schema: json!({}) },
            OpDef { name: "get_item".into(), description: "g".into(), read_only: true, args_schema: json!({}) },
            OpDef { name: "create_item".into(), description: "c".into(), read_only: false, args_schema: json!({}) },
        ],
        connect_guidance: "Start the local mock server.".into(),
    };
    let env = HashMap::from([
        ("HATCH_MOCKHTTP_BASE_URL".to_string(), server.base_url.clone()),
        ("CONNECTOR_TOKEN".to_string(), "test-token-mock".to_string()),
    ]);
    (m, env)
}

#[test]
fn mockhttp_connector_end_to_end_via_runner() {
    let server = MockServer::start();
    let (m, env) = mock_manifest(&server);
    let s = runner::status(&m, &env);
    assert!(s.ok, "status should pass: {}", s.detail);

    let r = runner::call(&m, "get_item", &json!({"id": "42"}), &env).unwrap();
    assert_eq!(r["name"], "thing");

    let r = runner::call(&m, "create_item", &json!({"name": "new"}), &env).unwrap();
    assert_eq!(r["id"], "1");
    assert_eq!(r["name"], "new");

    let e = runner::call(&m, "get_item", &json!({"id": "99"}), &env).unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");
}

#[test]
fn mockhttp_connector_auth_failure_is_a_clean_error() {
    let server = MockServer::start();
    let (m, mut env) = mock_manifest(&server);
    env.insert("CONNECTOR_TOKEN".to_string(), "test-token-wrong".to_string());
    let e = runner::call(&m, "get_item", &json!({"id": "42"}), &env).unwrap_err();
    assert!(e.to_string().contains("auth_missing"), "{e}");
    assert!(!e.to_string().contains("test-token-wrong"), "token must not leak into errors");
}

#[test]
fn mockhttp_connector_reports_not_configured_without_base_url() {
    let m = ConnectorManifest {
        name: "mockhttp".into(),
        command: mockhttp_bin().to_string_lossy().into_owned(),
        env: vec!["HATCH_MOCKHTTP_BASE_URL".into()],
        ops: vec![OpDef { name: "status".into(), ..Default::default() }],
        ..Default::default()
    };
    let s = runner::status(&m, &HashMap::new());
    assert!(!s.ok);
    assert!(s.detail.contains("not_configured"), "{}", s.detail);
}

#[test]
fn registry_loads_manifests_from_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("notes.toml"),
        "name = \"notes\"\ncommand = \"hatch-notes-connector\"\n[[ops]]\nname = \"status\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("broken.toml"), "name = \n").unwrap();
    let (manifests, warnings) = load_dir(dir.path());
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].name, "notes");
    assert_eq!(warnings.len(), 1);
}
