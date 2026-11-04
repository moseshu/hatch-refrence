//! CLI end-to-end for inference usage: seeded ledger → `hatch
//! inference usage` (all / --day / --model), empty home, and the
//! proxy refusing to start without an upstream.

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

fn seed_ledger(home: &std::path::Path) {
    let dir = home.join("inference");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("usage.jsonl"),
        "{\"ts\":\"2026-11-05T10:00:00Z\",\"model\":\"gpt-6-luna\",\"prompt_tokens\":100,\"completion_tokens\":20,\"cached_tokens\":30,\"reasoning_tokens\":0,\"duration_ms\":100,\"retries\":0,\"status\":200,\"ok\":true}\n\
         {\"ts\":\"2026-11-04T09:00:00Z\",\"model\":\"other\",\"prompt_tokens\":50,\"completion_tokens\":5,\"cached_tokens\":0,\"reasoning_tokens\":0,\"duration_ms\":50,\"retries\":0,\"status\":200,\"ok\":true}\n",
    )
    .unwrap();
}

#[test]
fn usage_summarizes_seeded_ledger() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[inference]\nprice_input_per_million = 10.0\nprice_output_per_million = 40.0\n",
    )
    .unwrap();
    seed_ledger(home.path());
    let out = stdout(&run(home.path(), &["inference", "usage"]));
    assert!(out.contains("2 次调用"), "{out}");
    assert!(out.contains("prompt 150 tokens"), "{out}");
    assert!(out.contains("completion 25 tokens"), "{out}");
    // Cost: 150/1e6*10 + 25/1e6*40 = 0.0025.
    assert!(out.contains("0.0025 CNY"), "{out}");
    let out = stdout(&run(
        home.path(),
        &["inference", "usage", "--day", "2026-11-05"],
    ));
    assert!(out.contains("1 次调用"), "{out}");
    assert!(out.contains("prompt 100 tokens"), "{out}");
    let out = stdout(&run(
        home.path(),
        &["inference", "usage", "--model", "other"],
    ));
    assert!(out.contains("1 次调用"), "{out}");
    assert!(out.contains("prompt 50 tokens"), "{out}");
}

#[test]
fn usage_with_empty_home_and_no_prices() {
    let home = tempfile::tempdir().unwrap();
    let out = stdout(&run(home.path(), &["inference", "usage"]));
    assert!(out.contains("暂无推理用量记录"), "{out}");
}

#[test]
fn proxy_refuses_to_start_without_upstream() {
    let home = tempfile::tempdir().unwrap();
    let out = run(home.path(), &["inference", "proxy"]);
    assert!(
        !out.status.success(),
        "proxy must not start without an upstream"
    );
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("上游"), "{err}");
}
