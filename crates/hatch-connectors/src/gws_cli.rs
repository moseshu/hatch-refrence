//! The shared `hatch_gws_cli` helper runner: Gmail and Google
//! Calendar both shell out to the same binary, which owns the
//! connector's OAuth grant. This module never sees a key, a token or
//! a password — it only passes argv to the helper binary and returns
//! its stdout. The helper path comes from `HATCH_GWS_CLI` (default
//! `/opt/hatch/bin/hatch_gws_cli`).
//!
//! Split out of `mail::gmail` when Google Calendar writes joined:
//! the subprocess discipline (no shell, stdout/stderr drained on
//! threads, killed on timeout) is one mechanism, not two copies.

use hatch_core::{Error, Result};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default helper binary (the connector's documented entry point).
pub const DEFAULT_GWS_CLI: &str = "/opt/hatch/bin/hatch_gws_cli";

/// Helper binary path: `HATCH_GWS_CLI` wins, else the default.
pub fn cli_path() -> PathBuf {
    std::env::var("HATCH_GWS_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_GWS_CLI))
}

/// How the helper is invoked. The production runner shells out to
/// the helper binary; tests substitute canned JSON.
pub trait CliRunner: Send + Sync {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<String>;
}

/// Runs the helper binary as a subprocess (no shell, so subjects and
/// bodies need no quoting). Stdout/stderr are drained on threads so a
/// large reply cannot deadlock the worker, and the call is killed on
/// timeout instead of hanging the worker forever.
pub struct ProcessRunner {
    cli: PathBuf,
}

impl ProcessRunner {
    pub fn new(cli: PathBuf) -> Self {
        ProcessRunner { cli }
    }
}

impl CliRunner for ProcessRunner {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<String> {
        let mut child = Command::new(&self.cli)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Connector(format!("启动 helper 失败：{e}")))?;
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let out_thread = std::thread::spawn(move || read_all(&mut out_pipe).unwrap_or_default());
        let err_thread = std::thread::spawn(move || read_all(&mut err_pipe).unwrap_or_default());
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        // The helper was killed mid-flight: the
                        // effect may already have happened, so this
                        // is an unknown outcome, not a definite
                        // failure.
                        return Err(Error::UnknownOutcome(
                            "helper 超时（已终止）：操作可能已生效，请核对".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(Error::Connector(format!("等待 helper 失败：{e}"))),
            }
        }
        let status = child
            .wait()
            .map_err(|e| Error::Connector(format!("等待 helper 失败：{e}")))?;
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        if !status.success() {
            return Err(Error::Connector(format!(
                "helper 退出码 {status}：{}",
                String::from_utf8_lossy(&stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }
}

fn read_all(pipe: &mut Option<impl Read>) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(p) = pipe.as_mut() {
        p.read_to_end(&mut buf)?;
    }
    Ok(buf)
}
