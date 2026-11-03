//! Runner: spawn a connector as a subprocess CLI and speak the runner
//! protocol to it.
//!
//! One call = one process invocation: write one JSON request line to the
//! child's stdin, read one JSON response line from its stdout. A call that
//! does not answer within the manifest timeout is killed — the whole
//! process group, so grandchildren die too — and reported as a timeout,
//! never a hang.
//!
//! Credential injection is deliberately *not* done here: the runner only
//! ever passes the env map its caller resolved. When a manifest names a
//! `token_ref`, the caller (the `connector_call` executor in hatch-tools)
//! asks the vault helper to spawn the child with the token injected into
//! the child's env only; see [`request_line`] / [`parse_stdout`] for the
//! shared framing that both spawn paths use.

use crate::framework::manifest::ConnectorManifest;
use crate::framework::protocol::{
    parse_response_line, ConnectorError, ConnectorRequest, ERR_AUTH, ERR_BAD_RESPONSE,
    ERR_NOT_CONFIGURED, ERR_TIMEOUT, ERR_UNKNOWN_OP,
};
use hatch_core::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What a finished child run produced.
#[derive(Clone, Debug)]
pub struct ChildOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

/// Build the single request line for an op, validating the op exists in
/// the manifest first. Unknown ops fail here, before any spawn.
pub fn request_line(manifest: &ConnectorManifest, op: &str, args: &Value) -> Result<String> {
    if manifest.op(op).is_none() {
        return Err(Error::Connector(format!(
            "connector {}: unknown op `{op}` ({ERR_UNKNOWN_OP})",
            manifest.name
        )));
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let req = ConnectorRequest::new(
        op,
        args.clone(),
        &format!("req-{}-{nanos}", std::process::id()),
    );
    req.to_line().map_err(Error::from)
}

/// Parse a child's captured stdout into the result value (or a coded
/// connector error). Stderr is only used when stdout is empty: a child
/// that died before answering reports its last stderr line, truncated, so
/// a crash is diagnosable without leaking unbounded output.
pub fn parse_stdout(stdout: &str, stderr: &str) -> Result<Value> {
    let first = stdout.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        let tail = stderr.lines().last().unwrap_or("").chars().take(200).collect::<String>();
        return Err(Error::Connector(format!(
            "connector produced no output{tail}",
            tail = if tail.is_empty() { String::new() } else { format!(" (stderr: {tail})") }
        )));
    }
    match parse_response_line(first) {
        Ok(resp) if resp.ok => Ok(resp.result.unwrap_or(Value::Null)),
        Ok(resp) => {
            let e = resp.error.unwrap_or_else(|| {
                ConnectorError::new(ERR_BAD_RESPONSE, "connector failed without an error")
            });
            Err(Error::Connector(format!("connector error {}: {}", e.code, e.message)))
        }
        Err(e) => Err(Error::Connector(format!("connector error {}: {}", e.code, e.message))),
    }
}

/// Check that every env var the manifest declares is present in `env`.
/// Missing ones are a clean `not_configured` error with the manifest's
/// guidance — never a spawned child that fails cryptically.
pub fn check_env(manifest: &ConnectorManifest, env: &HashMap<String, String>) -> Result<()> {
    let missing: Vec<&str> =
        manifest.env.iter().filter(|n| !env.contains_key(n.as_str())).map(String::as_str).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let guidance = if manifest.connect_guidance.is_empty() {
        String::new()
    } else {
        format!(" {}", manifest.connect_guidance)
    };
    Err(Error::Connector(format!(
        "connector {} is not configured ({ERR_NOT_CONFIGURED}): missing env var(s) {} — set them and retry.{guidance}",
        manifest.name,
        missing.join(", ")
    )))
}

/// Run one child process with `stdin_text` on stdin, capturing stdout and
/// stderr. On timeout the whole process group is killed (unix) and an
/// `ERR_TIMEOUT` error is returned.
pub fn run_child(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    stdin_text: &str,
    timeout: Duration,
) -> Result<ChildOutcome> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .envs(env.iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        // New session so the timeout kill can take the whole process group.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child: Child = cmd.spawn().map_err(|e| {
        Error::Connector(format!("connector: failed to spawn `{command}`: {e}"))
    })?;
    let pid = child.id();
    let mut stdin = child.stdin.take();
    let stdin_text = stdin_text.to_string();
    // A child that never reads stdin must not block the writer forever.
    std::thread::spawn(move || {
        if let Some(mut s) = stdin.take() {
            let _ = s.write_all(stdin_text.as_bytes());
            let _ = s.write_all(b"\n");
            let _ = s.flush();
        }
    });
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (out_tx, out_rx) = mpsc::channel::<String>();
    if let Some(out) = stdout {
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut buf = String::new();
            while r.read_line(&mut buf).map(|n| n > 0).unwrap_or(false) {
                let _ = out_tx.send(std::mem::take(&mut buf));
            }
        });
    }
    let (err_tx, err_rx) = mpsc::channel::<String>();
    if let Some(err) = stderr {
        std::thread::spawn(move || {
            let mut r = BufReader::new(err);
            let mut buf = String::new();
            while r.read_line(&mut buf).map(|n| n > 0).unwrap_or(false) {
                let _ = err_tx.send(std::mem::take(&mut buf));
            }
        });
    }
    // First stdout line, or timeout.
    let line = match out_rx.recv_timeout(timeout) {
        Ok(l) => l,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_group(pid);
            let _ = child.wait();
            return Err(Error::Connector(format!(
                "connector `{command}` timed out after {}s ({ERR_TIMEOUT})",
                timeout.as_secs()
            )));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => String::new(),
    };
    // Drain whatever else arrived, then wait for exit (bounded).
    let mut stdout_text = line;
    while let Ok(l) = out_rx.recv_timeout(Duration::from_millis(200)) {
        stdout_text.push_str(&l);
    }
    let mut stderr_text = String::new();
    while let Ok(l) = err_rx.try_recv() {
        stderr_text.push_str(&l);
    }
    // The child should exit promptly after answering; do not wait forever.
    let exit_code = wait_bounded(&mut child, Duration::from_secs(5));
    Ok(ChildOutcome { stdout: stdout_text, stderr: stderr_text, exit_code })
}

/// Wait up to `timeout` for the child, then kill it and wait again.
fn wait_bounded(child: &mut Child, timeout: Duration) -> Option<i32> {
    let pid = child.id();
    // `try_wait` in a loop is cheaper than a thread, but the child may be
    // stuck; poll briefly, then kill.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_group(pid);
                    return match child.wait() {
                        Ok(s) => s.code(),
                        Err(_) => None,
                    };
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Kill a whole process group on unix; the single child elsewhere.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        // Negative pid = process group. Best effort; errors ignored.
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Call one op on a connector via a plain spawn (no vault token).
pub fn call(
    manifest: &ConnectorManifest,
    op: &str,
    args: &Value,
    env: &HashMap<String, String>,
) -> Result<Value> {
    check_env(manifest, env)?;
    let line = request_line(manifest, op, args)?;
    let out = run_child(&manifest.command, &manifest.args, env, &line, manifest.timeout())?;
    parse_stdout(&out.stdout, &out.stderr)
}

/// The result of a status check: never a hang, never a promise.
#[derive(Clone, Debug)]
pub struct StatusCheck {
    pub name: String,
    /// Whether the connector answered its `status` op successfully.
    pub ok: bool,
    /// One line: what the check found (or the failure, coded).
    pub detail: String,
    /// Guidance for the user when `ok` is false.
    pub guidance: String,
}

/// Run a connector's status check: call its `status` op when it declares
/// one. A connector with no `status` op, a missing env var, a timeout, or
/// any other failure reports `ok: false` with guidance — never a hang and
/// never a promise of results.
pub fn status(manifest: &ConnectorManifest, env: &HashMap<String, String>) -> StatusCheck {
    let guidance = if manifest.connect_guidance.is_empty() {
        format!(
            "Connector `{}` is not configured. See its manifest for how to connect it.",
            manifest.name
        )
    } else {
        manifest.connect_guidance.clone()
    };
    if manifest.op("status").is_none() {
        return StatusCheck {
            name: manifest.name.clone(),
            ok: false,
            detail: "connector declares no `status` op".into(),
            guidance,
        };
    }
    if let Err(e) = check_env(manifest, env) {
        return StatusCheck {
            name: manifest.name.clone(),
            ok: false,
            detail: e.to_string(),
            guidance,
        };
    }
    match call(manifest, "status", &Value::Object(Default::default()), env) {
        Ok(v) => {
            let detail = v
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| v.get("status").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "status ok".to_string());
            StatusCheck { name: manifest.name.clone(), ok: true, detail, guidance: String::new() }
        }
        Err(e) => StatusCheck {
            name: manifest.name.clone(),
            ok: false,
            detail: e.to_string(),
            guidance,
        },
    }
}

/// Is this error message an auth failure (used by callers to give auth
/// guidance instead of retrying blindly)?
pub fn is_auth_error(message: &str) -> bool {
    message.contains(ERR_AUTH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::manifest::OpDef;

    fn manifest_with_status() -> ConnectorManifest {
        ConnectorManifest {
            name: "t".into(),
            command: "true".into(),
            ops: vec![OpDef {
                name: "status".into(),
                description: String::new(),
                read_only: true,
                args_schema: Value::Object(Default::default()),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn request_line_rejects_unknown_op() {
        let m = manifest_with_status();
        assert!(request_line(&m, "nope", &Value::Null).is_err());
    }

    #[test]
    fn parse_stdout_ok_and_error() {
        assert_eq!(
            parse_stdout("{\"ok\":true,\"result\":42}\n", "").unwrap(),
            Value::from(42)
        );
        let e = parse_stdout(
            "{\"ok\":false,\"error\":{\"code\":\"not_found\",\"message\":\"gone\"}}\n",
            "",
        )
        .unwrap_err();
        assert!(e.to_string().contains("not_found"));
    }

    #[test]
    fn parse_stdout_empty_reports_stderr_tail() {
        let e = parse_stdout("", "boom: something failed\n").unwrap_err();
        assert!(e.to_string().contains("boom"));
    }

    #[test]
    fn check_env_reports_missing() {
        let mut m = manifest_with_status();
        m.env = vec!["MISSING_VAR".into()];
        let err = check_env(&m, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("not_configured"));
    }

    #[test]
    fn status_without_status_op_is_clean_failure() {
        let mut m = manifest_with_status();
        m.ops.clear();
        m.ops.push(OpDef {
            name: "x".into(),
            ..Default::default()
        });
        let s = status(&m, &HashMap::new());
        assert!(!s.ok);
        assert!(s.detail.contains("no `status` op"));
        assert!(!s.guidance.is_empty());
    }

    #[test]
    fn spawn_missing_command_is_clean_error() {
        let m = ConnectorManifest {
            name: "t".into(),
            command: "/nonexistent/definitely-not-a-command".into(),
            ops: vec![OpDef { name: "status".into(), ..Default::default() }],
            ..Default::default()
        };
        assert!(super::call(&m, "status", &Value::Null, &HashMap::new()).is_err());
    }
}
