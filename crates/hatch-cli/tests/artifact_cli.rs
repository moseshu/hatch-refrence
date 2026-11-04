//! CLI end-to-end for artifact quota and shares: build, quota,
//! share (approval) → approve (token minted once) → shares →
//! revoke → delete, plus a quota refusal. The HTTP serving of the
//! share link itself is covered by the in-process API tests.

use std::path::PathBuf;
use std::process::{Command, Output};

/// Locate the `hatch` binary (same pattern as api_server_e2e.rs).
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

fn run(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(hatch_bin())
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "command failed: {s}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    s
}

/// Extract an id (`art_` / `apr_` / `shr_` + 16 hex = 20 chars) from
/// command output, ignoring trailing punctuation.
fn take_id(out: &str, prefix: &str) -> String {
    let start = out
        .find(prefix)
        .unwrap_or_else(|| panic!("{prefix} not found in output: {out}"));
    out[start..start + prefix.len() + 16].to_string()
}

#[test]
fn artifact_quota_shares_revoke_delete() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // Build one artifact, check quota shows it.
    let out = stdout(&run(
        home,
        &["artifact", "build", "周报", "--content", "# 第一周"],
    ));
    let id = take_id(&out, "art_");
    let out = stdout(&run(home, &["artifact", "quota"]));
    assert!(out.contains("总数：1 / 200"), "{out}");
    assert!(out.contains("总字节："), "{out}");
    // Share submits an approval; approving it mints the token once.
    let out = stdout(&run(home, &["artifact", "share", &id]));
    let approval = take_id(&out, "apr_");
    let out = stdout(&run(home, &["approvals", "approve", &approval]));
    let token = out
        .split("/v1/shared/")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("share link with token in approve output: {out}")
        .to_string();
    assert!(token.starts_with("hatch_art_"), "{token}");
    // Shares lists the share (id + status, never the token).
    let out = stdout(&run(home, &["artifact", "shares"]));
    assert!(out.contains(&id), "{out}");
    assert!(out.contains("有效"), "{out}");
    assert!(
        !out.contains(&token),
        "token must never appear in the shares list"
    );
    let share_id = take_id(&out, "shr_");
    // Revoke by share id; a second revoke of the same id fails.
    let out = stdout(&run(home, &["artifact", "revoke", &share_id]));
    assert!(out.contains("已撤销分享"), "{out}");
    let out = run(home, &["artifact", "revoke", &share_id]);
    assert!(!out.status.success(), "revoking twice must fail");
    // The artifact is unshared again, so it cannot deliver.
    let out = run(home, &["artifact", "deliver", &id]);
    assert!(!out.status.success(), "revoked artifact must not deliver");
    // Delete removes the artifact.
    let out = stdout(&run(home, &["artifact", "delete", &id]));
    assert!(out.contains("已删除 artifact"), "{out}");
    let out = run(home, &["artifact", "show", &id]);
    assert!(!out.status.success(), "deleted artifact is gone");
}

#[test]
fn artifact_quota_refusal_over_cli() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::write(home.join("config.toml"), "[artifacts]\nmax_artifacts = 1\n").unwrap();
    let out = stdout(&run(home, &["artifact", "build", "一", "--content", "x"]));
    assert!(out.contains("已创建 artifact"), "{out}");
    let out = run(home, &["artifact", "build", "二", "--content", "y"]);
    assert!(
        !out.status.success(),
        "second artifact must break the count limit"
    );
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("1/1"), "refusal names usage and cap: {err}");
    // Only the first artifact exists: the refusal wrote nothing.
    let out = stdout(&run(home, &["artifact", "list"]));
    assert_eq!(out.lines().count(), 1, "{out}");
}
