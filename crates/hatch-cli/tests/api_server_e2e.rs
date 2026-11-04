//! End-to-end: spawn the real `hatch daemon run --api` as a
//! subprocess and drive it over real HTTP: health (no token),
//! status + session creation (401 without a token). The daemon is
//! stopped gracefully with `hatch daemon stop` afterwards.
//! No network beyond loopback, no model, no real mail.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Locate the `hatch` binary: `CARGO_BIN_EXE_hatch` when cargo sets it,
/// otherwise build it on demand. (Same pattern as mcp_server_e2e.rs.)
fn hatch_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_hatch") {
        return p.into();
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let plain = manifest.join("../target/debug/hatch");
    if plain.is_file() {
        return plain;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = Command::new(&cargo)
        .args(["build", "-p", "hatch-cli", "--bin", "hatch"])
        .current_dir(manifest.parent().unwrap())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok && plain.is_file(), "could not build hatch binary");
    plain
}

/// One raw HTTP request over loopback; returns (status, body).
fn raw(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
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
    let status: u16 = head
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    (status, body.to_string())
}

/// The daemon prints `产品 API 已启动：http://127.0.0.1:<port>/v1` once
/// bound (port 0 = ephemeral). Scan the log file for that port.
fn api_port_from_log(text: &str) -> Option<u16> {
    let marker = "http://127.0.0.1:";
    let at = text.find(marker)? + marker.len();
    text[at..].split('/').next()?.parse().ok()
}

fn stop_daemon(exe: &PathBuf, home: &std::path::Path) {
    let out = Command::new(exe)
        .args([
            "--home",
            home.to_str().unwrap(),
            "daemon",
            "stop",
            "--wait-secs",
            "15",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "daemon stop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn api_server_binary_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    let exe = hatch_bin();
    let home_str = home.path().to_str().unwrap().to_string();
    let log_path = home.path().join("daemon.log");
    let log_file = std::fs::File::create(&log_path).unwrap();
    let mut child: Child = Command::new(&exe)
        .args([
            "--home",
            &home_str,
            "daemon",
            "run",
            "--api",
            "--api-bind",
            "127.0.0.1:0",
            "--tick-secs",
            "3600",
        ])
        // A token via env so the binary exercises the real auth path.
        .env("HATCH_DAEMON_API_TOKEN", "e2e-test-token")
        .stderr(Stdio::from(log_file))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // Wait for the API line (bounded; fail with the log on timeout).
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut port = None;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&log_path) {
            if let Some(p) = api_port_from_log(&text) {
                port = Some(p);
                break;
            }
        }
        match child.try_wait().unwrap() {
            Some(status) => panic!(
                "daemon exited early ({status}); log:\n{}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
            ),
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let port = port.expect("daemon did not print its API address in 30s");
    // Health needs no token; everything else requires the bearer token.
    let (status, body) = raw(port, "GET", "/v1/health", &[], "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"status\":\"ok\""), "{body}");
    let (status, body) = raw(port, "GET", "/v1/status", &[], "");
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("unauthorized"), "{body}");
    let (status, body) = raw(
        port,
        "GET",
        "/v1/status",
        &[("Authorization", "Bearer wrong")],
        "",
    );
    assert_eq!(status, 401, "{body}");
    let auth = [("Authorization", "Bearer e2e-test-token")];
    let (status, body) = raw(port, "GET", "/v1/status", &auth, "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"model_configured\":false"), "{body}");
    // A real write through the binary: create a session, list it back.
    let (status, body) = raw(port, "POST", "/v1/sessions", &auth, r#"{"title":"e2e"}"#);
    assert_eq!(status, 201, "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = raw(port, "GET", "/v1/sessions", &auth, "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&id), "{body}");
    // Stop gracefully, then reap the child.
    stop_daemon(&exe, home.path());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                panic!("daemon did not exit after `daemon stop`");
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}
