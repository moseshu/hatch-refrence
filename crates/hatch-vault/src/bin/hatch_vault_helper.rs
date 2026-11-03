//! `hatch-vault-helper` — the vault helper process.
//!
//! Spawned by [`hatch_vault::VaultClient`] (never by hand in normal use).
//! Holds token material in its own memory and speaks NDJSON on stdin /
//! stdout. Dies with its parent, on idle timeout, or on `shutdown`.
//!
//! Usage: `hatch-vault-helper --home <dir> [--parent-pid <pid>]
//! [--idle-timeout-secs <n>]`

use hatch_vault::helper::Helper;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Die with SIGKILL when the parent goes away (called by the helper on
/// itself at startup; the client also verifies the parent pid).
#[cfg(unix)]
fn die_with_parent() {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
    }
}

/// Run the helper: read NDJSON requests on stdin, write NDJSON responses
/// on stdout. Returns when told to shut down, when the parent dies, or
/// after the idle timeout.
fn run(home: &Path, idle_timeout_secs: u64, parent_pid: Option<i32>) -> i32 {
    #[cfg(unix)]
    die_with_parent();
    let helper = Arc::new(Helper::load(home, idle_timeout_secs, parent_pid));
    // Pid file so `hatch vault stop` / `status` can find this process.
    let pid_path = hatch_vault::vault_dir(home).join("helper.pid");
    let _ = std::fs::create_dir_all(hatch_vault::vault_dir(home));
    let _ = std::fs::write(&pid_path, std::process::id().to_string());
    let watcher = helper.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        if watcher.should_exit() {
            std::process::exit(0);
        }
    });
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let id = helper.next_id();
        let resp = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(req) => match helper.handle(&req) {
                Ok(result) => json!({"id": id, "ok": true, "result": result}),
                Err((code, message)) => {
                    json!({"id": id, "ok": false, "error": {"code": code, "message": message}})
                }
            },
            Err(_) => json!({"id": id, "ok": false, "error": {"code": "invalid_args", "message": "request is not valid JSON"}}),
        };
        if writeln!(out, "{}", serde_json::to_string(&resp).unwrap_or_default()).is_err() {
            break;
        }
        let _ = out.flush();
        if helper.is_shutting_down() {
            break;
        }
    }
    let _ = std::fs::remove_file(&pid_path);
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut home: Option<PathBuf> = None;
    let mut parent_pid: Option<i32> = None;
    let mut idle_timeout_secs: u64 = hatch_vault::client::DEFAULT_IDLE_TIMEOUT_SECS;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--home" => {
                home = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "--parent-pid" => {
                parent_pid = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "--idle-timeout-secs" => {
                idle_timeout_secs =
                    args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(idle_timeout_secs);
                i += 2;
            }
            other => {
                eprintln!("hatch-vault-helper: unknown argument `{other}`");
                std::process::exit(2);
            }
        }
    }
    let Some(home) = home else {
        eprintln!("hatch-vault-helper: --home is required");
        std::process::exit(2);
    };
    std::process::exit(run(&home, idle_timeout_secs, parent_pid));
}
