//! Integration tests for the full vault: helper process, surrogate-only
//! parent, injection into children, dies-with-parent, perms, and helper
//! lifecycle. No real network, no real credentials — every token is an
//! obviously fake `test-token-...` value. (HTTP / refresh / OAuth paths
//! live in `vault_oauth.rs` against a local mock server.)

use hatch_vault::client::{helper_status, stop_helper, StoreRequest, VaultClient};
use hatch_vault::open;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn start_client(home: &std::path::Path) -> VaultClient {
    VaultClient::start(home).expect("start vault helper")
}

#[test]
fn helper_spawn_ping_store_list_delete() {
    let home = temp_home();
    let client = start_client(home.path());
    assert!(client.is_alive());
    let ping = client.ping().unwrap();
    assert_eq!(ping["token_count"], 0);

    client
        .store(
            "mock-token",
            StoreRequest {
                value: "test-token-abc".into(),
                connector: Some("mockhttp".into()),
                ..Default::default()
            },
        )
        .unwrap();
    // List returns metadata only: the summary serializes with no value.
    let items = client.list().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "mock-token");
    assert_eq!(items[0].connector.as_deref(), Some("mockhttp"));
    assert!(!items[0].expired);
    let text = serde_json::to_string(&items).unwrap();
    assert!(!text.contains("test-token-abc"), "list must never expose values");

    assert!(client.delete("mock-token").unwrap());
    assert!(client.list().unwrap().is_empty());
    assert!(!client.delete("mock-token").unwrap());
}

#[test]
fn file_perms_stay_0600_0700_after_helper_store() {
    let home = temp_home();
    let client = start_client(home.path());
    client
        .store("k", StoreRequest { value: "test-token-abc".into(), ..Default::default() })
        .unwrap();
    drop(client);
    let vault = open(home.path()).unwrap();
    assert_eq!(
        std::fs::metadata(&vault.dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&vault.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(vault.check_permissions().is_empty());
    // The file store can still read what the helper wrote (legacy path).
    assert_eq!(vault.get("k").unwrap(), Some("test-token-abc".into()));
}

#[test]
fn inject_runs_child_with_token_parent_never_sees_value() {
    let home = temp_home();
    let client = start_client(home.path());
    client
        .store("mock-token", StoreRequest { value: "test-token-abc".into(), ..Default::default() })
        .unwrap();
    // The child checks the token itself and prints only MATCH/MISMATCH, so
    // the value never crosses back to the parent in any form.
    let out = client
        .inject(
            "mock-token",
            "CONNECTOR_TOKEN",
            "sh",
            &["-c".into(), "test \"$CONNECTOR_TOKEN\" = \"test-token-abc\" && echo MATCH || echo MISMATCH".into()],
            "",
            10,
        )
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert!(out.stdout.contains("MATCH"), "stdout: {}", out.stdout);
    assert!(!out.stdout.contains("test-token-abc"));
    assert!(!out.stderr.contains("test-token-abc"));
}

#[test]
fn inject_missing_token_is_a_clean_error() {
    let home = temp_home();
    let client = start_client(home.path());
    let e = client
        .inject("nope", "CONNECTOR_TOKEN", "true", &[], "", 5)
        .unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");
}

#[test]
fn helper_status_and_stop() {
    let home = temp_home();
    assert!(!helper_status(home.path()).running);
    let client = start_client(home.path());
    let st = helper_status(home.path());
    assert!(st.running);
    assert!(st.pid.is_some());
    drop(client);
    // Drop asks the helper to stop; give it a moment, then force-stop.
    std::thread::sleep(Duration::from_millis(300));
    if helper_status(home.path()).running {
        assert!(stop_helper(home.path()));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(!helper_status(home.path()).running);
}

#[test]
fn helper_dies_when_monitored_parent_dies() {
    // Spawn a sleep as the "parent" to monitor, then a helper watching it.
    let mut sleep = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let sleep_pid = sleep.id() as i32;
    let home = temp_home();
    open(home.path()).unwrap();
    let helper_bin = hatch_vault::client::helper_path();
    let mut helper = std::process::Command::new(&helper_bin)
        .args([
            "--home",
            &home.path().to_string_lossy(),
            "--parent-pid",
            &sleep_pid.to_string(),
            "--idle-timeout-secs",
            "600",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn helper");
    // Ping it once to prove it is up.
    {
        let mut stdin = helper.stdin.take().unwrap();
        let stdout = helper.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            let mut line = String::new();
            while r.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                let _ = tx.send(std::mem::take(&mut line));
            }
        });
        writeln!(stdin, "{{\"op\":\"ping\"}}").unwrap();
        stdin.flush().unwrap();
        let line = rx.recv_timeout(Duration::from_secs(5)).expect("helper ping reply");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], true);
        // Keep stdin open in this scope so the helper does not see EOF.
        std::mem::forget(stdin);
    }
    let _ = sleep.kill();
    let _ = sleep.wait();
    // The helper polls its parent every 500ms; it must exit promptly.
    let start = std::time::Instant::now();
    loop {
        match helper.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "helper did not die with its parent"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("try_wait: {e}"),
        }
    }
}

#[test]
fn helper_exits_after_idle_timeout() {
    let home = temp_home();
    open(home.path()).unwrap();
    let helper_bin = hatch_vault::client::helper_path();
    let mut helper = std::process::Command::new(&helper_bin)
        .args([
            "--home",
            &home.path().to_string_lossy(),
            "--idle-timeout-secs",
            "2",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn helper");
    let start = std::time::Instant::now();
    loop {
        match helper.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success());
                break;
            }
            Ok(None) => {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "helper did not exit on idle timeout"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("try_wait: {e}"),
        }
    }
}
