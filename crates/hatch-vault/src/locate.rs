//! Locating and stopping the vault helper process: binary resolution,
//! pid file, liveness check.
//!
//! The helper binary is resolved as: `HATCH_VAULT_HELPER`, then the
//! `CARGO_BIN_EXE_hatch-vault-helper` test env var, then a binary next to
//! the current executable, then the debug build (built on demand in tests),
//! then `PATH`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Pid file the helper writes on start (`<home>/vault/helper.pid`).
pub fn pid_file(home: &Path) -> PathBuf {
    crate::vault_dir(home).join("helper.pid")
}

/// The helper's pid from its pid file, if the process is still alive.
pub fn helper_pid(home: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(pid_file(home)).ok()?;
    let pid: i32 = text.trim().parse().ok()?;
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid, 0) == 0 {
            Some(pid)
        } else {
            None
        }
    }
    #[cfg(not(unix))]
    {
        Some(pid)
    }
}

/// Whether the helper is running for this home, and its pid.
#[derive(Clone, Debug)]
pub struct HelperStatus {
    pub running: bool,
    pub pid: Option<i32>,
}

pub fn helper_status(home: &Path) -> HelperStatus {
    let pid = helper_pid(home);
    HelperStatus { running: pid.is_some(), pid }
}

/// Stop a running helper for this home (SIGTERM; the pid file is cleaned
/// up by the helper on exit). Returns `true` when a live helper was found.
pub fn stop_helper(home: &Path) -> bool {
    let Some(pid) = helper_pid(home) else {
        return false;
    };
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
    // Best effort: wait briefly for the pid file to disappear.
    for _ in 0..20 {
        if helper_pid(home).is_none() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// Locate the helper binary: `HATCH_VAULT_HELPER`, then the
/// `CARGO_BIN_EXE_hatch-vault-helper` test env var, then a binary next to
/// the current executable, then `PATH`. When the expected debug binary is
/// missing it is built on demand (test convenience, same pattern as the
/// MCP fake server).
pub fn helper_path() -> PathBuf {
    if let Ok(p) = std::env::var("HATCH_VAULT_HELPER") {
        if !p.is_empty() {
            return p.into();
        }
    }
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_hatch-vault-helper") {
        return p.into();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("hatch-vault-helper");
            if cand.is_file() {
                return cand;
            }
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plain = manifest.join("../../target/debug/hatch-vault-helper");
    if plain.is_file() {
        return plain;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = Command::new(&cargo)
        .args(["build", "-p", "hatch-vault", "--bin", "hatch-vault-helper"])
        .current_dir(manifest.parent().unwrap().parent().unwrap())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && plain.is_file() {
        return plain;
    }
    PathBuf::from("hatch-vault-helper")
}

