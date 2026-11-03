//! End-to-end: spawn the real `hatch mcp serve` as a subprocess and drive
//! it with the hatch-mcp stdio client — list, a read-only call, a queued
//! approval, poll, approve out of band, poll again. No network, no real
//! mail (the outbox fixture receives the approved mail).

use hatch_mcp::{Client, McpServerConfig};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// Locate the `hatch` binary: `CARGO_BIN_EXE_hatch` when cargo sets it,
/// otherwise build it on demand.
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

fn outbox_count(home: &std::path::Path) -> usize {
    // One mail = one `.eml` file plus the `outbox.jsonl` index; count mails.
    std::fs::read_dir(home.join("outbox"))
        .map(|d| {
            d.filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "eml"))
            })
            .count()
        })
        .unwrap_or(0)
}

#[test]
fn mcp_server_stdio_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    let exe = hatch_bin();
    let home_str = home.path().to_str().unwrap().to_string();
    let cfg = McpServerConfig {
        name: "hatch".into(),
        command: exe.to_string_lossy().into_owned(),
        args: vec![
            "--home".into(),
            home_str.clone(),
            "mcp".into(),
            "serve".into(),
        ],
        timeout_secs: 15,
        enabled: true,
        ..Default::default()
    };
    let mut c = Client::connect(&cfg, &HashMap::new(), None).expect("connect to hatch mcp serve");
    c.ping().unwrap();

    let tools = c.list_tools().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"memory_search"), "{names:?}");
    assert!(names.contains(&"approval_status"), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("mcp_")), "{names:?}");

    // Read-only call round-trips.
    let r = c
        .call_tool("memory_search", json!({"query": "zzz-no-such-memory"}))
        .unwrap();
    assert_eq!(r["structuredContent"]["status"], "ok");
    assert_eq!(r["content"][0]["text"], "没有找到相关记忆。");

    // Side-effecting call queues an approval and does NOT execute.
    let r = c
        .call_tool(
            "propose_email",
            json!({"to": ["boss@corp.com"], "subject": "周报", "body": "见附件"}),
        )
        .unwrap();
    assert_eq!(r["structuredContent"]["status"], "approval_required", "{r}");
    let id = r["structuredContent"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(outbox_count(home.path()), 0, "nothing sent before approval");

    // Poll: pending.
    let r = c.call_tool("approval_status", json!({"id": id})).unwrap();
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pending"),
        "{r}"
    );

    // The user approves out of band (separate process, as a human would).
    let ok = Command::new(&exe)
        .args(["--home", &home_str, "approvals", "approve", &id])
        .status()
        .expect("run hatch approvals approve")
        .success();
    assert!(ok, "hatch approvals approve must succeed");
    assert_eq!(
        outbox_count(home.path()),
        1,
        "approved mail lands in the outbox"
    );

    // Poll again: executed.
    let r = c.call_tool("approval_status", json!({"id": id})).unwrap();
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("executed"), "{text}");
}
