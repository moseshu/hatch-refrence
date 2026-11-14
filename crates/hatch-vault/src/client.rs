//! Parent-side client for the vault helper process.
//!
//! [`VaultClient`] spawns `hatch-vault-helper` once and talks NDJSON to it
//! over pipes. The client holds **no token values**: `store` is the one
//! call where a value crosses into the client (the user explicitly typed
//! it for `hatch vault set`, or an OAuth flow just received it), and it is
//! dropped as soon as the request line is written. Every other operation
//! works on names only, or asks the helper to use the value itself.
//!
//! [`VaultClient::reveal`] is the documented exception: it returns a value
//! over the pipe for the spawn instant of a long-lived stdio child (an MCP
//! server), which needs parent-owned pipes. Callers must drop the value
//! immediately after spawning. The one-shot connector path
//! ([`VaultClient::inject`]) never reveals.

use crate::record::TokenSummary;
use hatch_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default idle timeout for the helper (seconds).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

pub use crate::locate::{helper_path, helper_pid, helper_status, stop_helper, HelperStatus};

struct Inner {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    next_id: u64,
}

/// A live vault helper process. Cloneable: all clones share one child.
#[derive(Clone)]
pub struct VaultClient {
    home: PathBuf,
    inner: Arc<Mutex<Inner>>,
    timeout: Duration,
}

/// Outcome of an `inject` run: what the child produced.
#[derive(Clone, Debug)]
pub struct InjectOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Outcome of a helper-performed HTTP call.
#[derive(Clone, Debug)]
pub struct HttpOutcome {
    pub status: u16,
    pub body: String,
}

/// What to store for a token.
#[derive(Clone, Debug, Default)]
pub struct StoreRequest {
    pub value: String,
    pub connector: Option<String>,
    /// The account the token was issued for, if known.
    pub account: Option<String>,
    /// The scopes granted with the token, if known.
    pub scope: Option<String>,
    /// Token/refresh endpoint recorded at store time; refresh must talk
    /// to this provider's host.
    pub provider_url: Option<String>,
    /// Revocation endpoint, if the provider has one.
    pub revoke_url: Option<String>,
    pub expires_at: Option<i64>,
    pub refresh_token: Option<String>,
    pub refresh_supported: bool,
    pub refresh_url: Option<String>,
}

impl VaultClient {
    /// Spawn the helper for `home`. The helper dies with this process
    /// (`PR_SET_PDEATHSIG` + parent-pid polling) and after the idle
    /// timeout unless used.
    pub fn start(home: &Path) -> Result<Self> {
        crate::open(home)?;
        let path = helper_path();
        let mut cmd = Command::new(&path);
        cmd.args([
            "--home",
            &home.to_string_lossy(),
            "--parent-pid",
            &std::process::id().to_string(),
            "--idle-timeout-secs",
            &DEFAULT_IDLE_TIMEOUT_SECS.to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        #[cfg(unix)]
        unsafe {
            // The child sets PDEATHSIG on itself too (helper::run); this
            // pre_exec is the same belt-and-braces pattern.
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| {
            Error::Connector(format!("failed to spawn vault helper `{}`: {e}", path.display()))
        })?;
        let stdin = child.stdin.take().ok_or_else(|| Error::Invalid("vault helper: no stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Invalid("vault helper: no stdout".into()))?;
        // Drain stderr so a chatty helper can never block on a full pipe.
        if let Some(err) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut r = BufReader::new(err);
                let mut buf = String::new();
                while r.read_line(&mut buf).map(|n| n > 0).unwrap_or(false) {
                    buf.clear();
                }
            });
        }
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            let mut line = String::new();
            while r.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                if tx.send(line.trim_end().to_string()).is_err() {
                    break;
                }
                line.clear();
            }
        });
        let client = VaultClient {
            home: home.to_path_buf(),
            inner: Arc::new(Mutex::new(Inner { child, stdin, rx, next_id: 1 })),
            timeout: Duration::from_secs(30),
        };
        client.ping()?;
        Ok(client)
    }

    /// Is the helper child still alive?
    pub fn is_alive(&self) -> bool {
        self.inner
            .lock()
            .map(|mut i| matches!(i.child.try_wait(), Ok(None)))
            .unwrap_or(false)
    }

    /// The home this client serves.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// One request → the `result` value. `timeout` overrides the default
    /// reply wait (used for long `inject` runs).
    fn call(&self, req: Value, timeout: Option<Duration>) -> Result<Value> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("vault client lock poisoned".into()))?;
        let id = inner.next_id;
        inner.next_id += 1;
        let mut req = req;
        req["id"] = json!(id);
        let line = serde_json::to_string(&req)?;
        inner.stdin.write_all(line.as_bytes())?;
        inner.stdin.write_all(b"\n")?;
        inner.stdin.flush()?;
        let wait = timeout.unwrap_or(self.timeout);
        let resp_line = inner.rx.recv_timeout(wait).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => {
                Error::Connector("vault helper timed out".into())
            }
            mpsc::RecvTimeoutError::Disconnected => {
                Error::Connector("vault helper stdout closed".into())
            }
        })?;
        let resp: Value = serde_json::from_str(&resp_line).map_err(|_| {
            Error::Invalid("vault helper returned malformed JSON".into())
        })?;
        if resp.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(resp.get("result").cloned().unwrap_or(Value::Null))
        } else {
            let err = resp.get("error").cloned().unwrap_or(Value::Null);
            let code = err.get("code").and_then(Value::as_str).unwrap_or("error");
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("?");
            Err(Error::Connector(format!("vault helper error {code}: {msg}")))
        }
    }

    pub fn ping(&self) -> Result<Value> {
        self.call(json!({"op": "ping"}), None)
    }

    /// Store a token in the helper (and its 0600 file). The value is
    /// dropped by the caller after this returns; the client keeps no copy.
    pub fn store(&self, name: &str, req: StoreRequest) -> Result<()> {
        if !crate::is_valid_name(name) {
            return Err(Error::Invalid(format!("invalid token name `{name}`")));
        }
        let mut body = json!({
            "op": "store",
            "name": name,
            "value": req.value,
            "refresh_supported": req.refresh_supported,
        });
        if let Some(c) = req.connector {
            body["connector"] = json!(c);
        }
        if let Some(a) = req.account {
            body["account"] = json!(a);
        }
        if let Some(s) = req.scope {
            body["scope"] = json!(s);
        }
        if let Some(u) = req.provider_url {
            body["provider_url"] = json!(u);
        }
        if let Some(u) = req.revoke_url {
            body["revoke_url"] = json!(u);
        }
        if let Some(e) = req.expires_at {
            body["expires_at"] = json!(e);
        }
        if let Some(r) = req.refresh_token {
            body["refresh_token"] = json!(r);
        }
        if let Some(u) = req.refresh_url {
            body["refresh_url"] = json!(u);
        }
        self.call(body, None)?;
        Ok(())
    }

    /// List token metadata: names and state only, never values.
    pub fn list(&self) -> Result<Vec<TokenSummary>> {
        let v = self.call(json!({"op": "list"}), None)?;
        let items = v.get("tokens").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for item in items {
            out.push(serde_json::from_value(item).map_err(Error::from)?);
        }
        Ok(out)
    }

    /// List token names only.
    pub fn list_names(&self) -> Result<Vec<String>> {
        Ok(self.list()?.into_iter().map(|s| s.name).collect())
    }

    pub fn delete(&self, name: &str) -> Result<bool> {
        let v = self.call(json!({"op": "delete", "name": name}), None)?;
        Ok(v.get("deleted").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Spawn `command` with the token as env var `env_var` of the child
    /// only, feed it `stdin_text`, and return what it produced. The parent
    /// never sees the token value.
    pub fn inject(
        &self,
        name: &str,
        env_var: &str,
        command: &str,
        args: &[String],
        stdin_text: &str,
        timeout_secs: u64,
    ) -> Result<InjectOutcome> {
        let v = self.call(
            json!({
                "op": "inject",
                "name": name,
                "env_var": env_var,
                "command": command,
                "args": args,
                "stdin_text": stdin_text,
                "timeout_secs": timeout_secs,
            }),
            Some(Duration::from_secs(timeout_secs + 10)),
        )?;
        Ok(InjectOutcome {
            exit_code: v.get("exit_code").and_then(Value::as_i64).map(|c| c as i32),
            stdout: v.get("stdout").and_then(Value::as_str).unwrap_or("").to_string(),
            stderr: v.get("stderr").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }

    /// Perform an HTTP call with the token as a Bearer header, inside the
    /// helper. Only the status and body come back.
    pub fn request(
        &self,
        name: &str,
        method: &str,
        url: &str,
        body: Option<Value>,
    ) -> Result<HttpOutcome> {
        let mut req = json!({"op": "request", "name": name, "method": method, "url": url});
        if let Some(b) = body {
            req["body"] = b;
        }
        let v = self.call(req, None)?;
        Ok(HttpOutcome {
            status: v.get("status").and_then(Value::as_u64).unwrap_or(0) as u16,
            body: v.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }

    /// Like [`VaultClient::request`] but with a raw text body and extra
    /// headers — the CalDAV path (PROPFIND / REPORT with DAV XML,
    /// PUT with an .ics document, DELETE). The token value stays in
    /// the helper; only status + body cross the pipe.
    pub fn request_raw(
        &self,
        name: &str,
        method: &str,
        url: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<HttpOutcome> {
        let mut req = json!({"op": "request", "name": name, "method": method, "url": url});
        if let Some(b) = body {
            req["body_text"] = json!(b);
        }
        if !headers.is_empty() {
            let mut h = serde_json::Map::new();
            for (k, v) in headers {
                h.insert((*k).to_string(), json!(v));
            }
            req["headers"] = Value::Object(h);
        }
        let v = self.call(req, None)?;
        Ok(HttpOutcome {
            status: v.get("status").and_then(Value::as_u64).unwrap_or(0) as u16,
            body: v.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }

    /// Refresh a token via its recorded refresh URL. Reports
    /// "refresh_unsupported" when the connector has no refresh flow.
    pub fn refresh(&self, name: &str) -> Result<()> {
        self.call(json!({"op": "refresh", "name": name}), None)?;
        Ok(())
    }

    /// Refresh with account/scope expectations: when the record has an
    /// account or granted scope recorded, they must match / cover the
    /// expectation, or the refresh is refused before any network call.
    pub fn refresh_with(
        &self,
        name: &str,
        account: Option<&str>,
        scope: Option<&str>,
    ) -> Result<()> {
        let mut body = json!({"op": "refresh", "name": name});
        if let Some(a) = account {
            body["account"] = json!(a);
        }
        if let Some(s) = scope {
            body["scope"] = json!(s);
        }
        self.call(body, None)?;
        Ok(())
    }

    /// Revoke a token: the provider endpoint is attempted when recorded,
    /// but the token is blocked in hatch either way. Returns the helper's
    /// result (`revoked`, `provider_revoked`, `provider_error`).
    pub fn revoke(&self, name: &str) -> Result<Value> {
        self.call(json!({"op": "revoke", "name": name}), None)
    }

    /// Metadata for one token (never the value).
    pub fn status(&self, name: &str) -> Result<TokenSummary> {
        let v = self.call(json!({"op": "status", "name": name}), None)?;
        serde_json::from_value(v).map_err(Error::from)
    }

    /// Return the token value over the private pipe. **Spawn-instant only**:
    /// the caller must drop the value immediately after spawning the child
    /// (long-lived stdio MCP servers). Prefer [`VaultClient::inject`] or
    /// [`VaultClient::request`] everywhere else.
    pub fn reveal(&self, name: &str) -> Result<String> {
        let v = self.call(json!({"op": "reveal", "name": name}), None)?;
        v.get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Invalid("vault helper reveal returned no value".into()))
    }

    /// Ask the helper to exit, then wait for the child. Best effort.
    pub fn stop(&self) {
        let _ = self.call(json!({"op": "shutdown"}), Some(Duration::from_secs(5)));
        if let Ok(mut inner) = self.inner.lock() {
            let _ = inner.child.wait();
        }
    }
}

impl Drop for VaultClient {
    fn drop(&mut self) {
        // Last clone out (the Arc being dropped still counts as one):
        // ask the helper to stop; the helper also dies with its parent
        // and on its idle timeout, so a missed shutdown is safe.
        if Arc::strong_count(&self.inner) == 1 {
            self.stop();
        }
    }
}

/// Resolve extra env vars (names → process env values) for a child spawn.
pub fn resolve_env_names(names: &[String]) -> (HashMap<String, String>, Vec<String>) {
    let mut map = HashMap::new();
    let mut missing = Vec::new();
    for name in names {
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => {
                map.insert(name.clone(), v);
            }
            _ => missing.push(name.clone()),
        }
    }
    (map, missing)
}
