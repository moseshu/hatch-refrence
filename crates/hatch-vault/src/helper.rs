//! The vault helper process.
//!
//! A separate process (`hatch-vault-helper`) that holds token material in
//! its own memory. The agent process never reads token values into its own
//! memory for the operations that matter:
//!
//! * `inject` — the helper spawns a child (a connector or MCP server)
//!   with the token set as an env var of the *child only*, runs it to
//!   completion, and returns exit code / stdout / stderr. The parent never
//!   sees the value.
//! * `request` — the helper performs an HTTP call with the token as a
//!   `Bearer` header and returns only status + body.
//! * `reveal` — the parent gets the value over the pipe. This exists only
//!   because a long-lived stdio MCP server needs parent-owned pipes; the
//!   caller must drop the value immediately after spawning the child.
//!   It is the one documented parent-memory exposure (see SECURITY.md).
//!
//! Transport is newline-delimited JSON over the helper's stdin/stdout
//! (pipes). UDS with fd-passing would remove the `reveal` path entirely;
//! that is listed as future work, not silently claimed here.
//!
//! The helper dies with its parent: it sets `PR_SET_PDEATHSIG` on itself
//! at startup, polls the expected parent pid, and exits after an idle
//! timeout (default 300s) or on an explicit `shutdown` op.

use crate::record::{now_unix, parse_store, TokenRecord};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Max bytes kept from a child's stdout/stderr or an HTTP body.
const MAX_CAPTURE: usize = 1024 * 1024;

/// State held by the helper process.
pub struct Helper {
    store_path: PathBuf,
    tokens: Mutex<BTreeMap<String, TokenRecord>>,
    started: Instant,
    last_request: Mutex<Instant>,
    shutdown: AtomicBool,
    idle_timeout: Duration,
    parent_pid: Option<i32>,
    next_id: AtomicU64,
}

fn trunc(s: &str) -> String {
    if s.len() <= MAX_CAPTURE {
        s.to_string()
    } else {
        format!("{}…[truncated]", &s[..MAX_CAPTURE])
    }
}

impl Helper {
    pub fn load(home: &Path, idle_timeout_secs: u64, parent_pid: Option<i32>) -> Self {
        let store_path = crate::vault_path(home);
        let tokens = std::fs::read_to_string(&store_path)
            .ok()
            .and_then(|t| parse_store(&t).ok())
            .unwrap_or_default();
        Helper {
            store_path,
            tokens: Mutex::new(tokens),
            started: Instant::now(),
            last_request: Mutex::new(Instant::now()),
            shutdown: AtomicBool::new(false),
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            parent_pid,
            next_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn save(&self) -> Result<(), String> {
        let map = self.tokens.lock().map_err(|_| "token lock poisoned")?;
        let text = serde_json::to_string(&*map).map_err(|e| e.to_string())?;
        let tmp = self
            .store_path
            .with_file_name(format!(".tokens.json.tmp.{}", std::process::id()));
        {
            let mut f = crate::create_file_0600(&tmp).map_err(|e| e.to_string())?;
            f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &self.store_path).map_err(|e| e.to_string())?;
        crate::tighten_file(&self.store_path);
        Ok(())
    }

    /// Path of the token store file (the lock file sits next to it).
    pub(crate) fn store_path(&self) -> &Path {
        &self.store_path
    }

    /// The in-memory token map. Lifecycle code locks it briefly around
    /// snapshots and applies, never across a network call.
    pub(crate) fn tokens(&self) -> &Mutex<BTreeMap<String, TokenRecord>> {
        &self.tokens
    }

    /// Re-read `tokens.json` into the map (another helper may have
    /// rotated a token since this one loaded). Call under the store
    /// lock; a corrupt file leaves the map untouched.
    pub(crate) fn reload(&self) -> Result<(), String> {
        let fresh = std::fs::read_to_string(&self.store_path)
            .ok()
            .and_then(|t| parse_store(&t).ok());
        if let Some(map) = fresh {
            let mut guard = self.tokens.lock().map_err(|_| "token lock poisoned")?;
            *guard = map;
        }
        Ok(())
    }

    fn touch(&self) {
        if let Ok(mut t) = self.last_request.lock() {
            *t = Instant::now();
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn idle_secs(&self) -> u64 {
        self.last_request.lock().map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }

    /// One request → one response value (already shaped as the `result`).
    pub fn handle(&self, req: &Value) -> Result<Value, (String, String)> {
        self.touch();
        let op = req.get("op").and_then(Value::as_str).unwrap_or("");
        match op {
            "ping" => Ok(json!({
                "pid": std::process::id(),
                "uptime_secs": self.started.elapsed().as_secs(),
                "token_count": self.tokens.lock().map(|m| m.len()).unwrap_or(0),
            })),
            "store" => {
                let name = str_field(req, "name")?;
                if !crate::is_valid_name(&name) {
                    return Err(("invalid_args".into(), "invalid token name".into()));
                }
                let value = str_field(req, "value")?;
                if value.is_empty() {
                    return Err(("invalid_args".into(), "token value must not be empty".into()));
                }
                let mut rec = TokenRecord::new(&value);
                rec.connector = req.get("connector").and_then(Value::as_str).map(str::to_string);
                rec.account = req.get("account").and_then(Value::as_str).map(str::to_string);
                rec.scope = req.get("scope").and_then(Value::as_str).map(str::to_string);
                rec.provider_url =
                    req.get("provider_url").and_then(Value::as_str).map(str::to_string);
                rec.revoke_url =
                    req.get("revoke_url").and_then(Value::as_str).map(str::to_string);
                rec.expires_at = req.get("expires_at").and_then(Value::as_i64);
                rec.refresh_token =
                    req.get("refresh_token").and_then(Value::as_str).map(str::to_string);
                rec.refresh_supported =
                    req.get("refresh_supported").and_then(Value::as_bool).unwrap_or(false);
                rec.refresh_url =
                    req.get("refresh_url").and_then(Value::as_str).map(str::to_string);
                // Under the store lock, so a concurrent rotation is never
                // clobbered by this store.
                crate::lifecycle::store_locked(self, &name, rec)?;
                Ok(json!({ "stored": name }))
            }
            "list" => {
                let now = now_unix();
                let items: Vec<Value> = self
                    .tokens
                    .lock()
                    .map_err(|_| err_pair("internal", "lock poisoned"))?
                    .iter()
                    .map(|(n, r)| {
                        serde_json::to_value(r.summary(n, now)).unwrap_or(Value::Null)
                    })
                    .collect();
                Ok(json!({ "tokens": items }))
            }
            "status" => {
                let name = str_field(req, "name")?;
                let map = self
                    .tokens
                    .lock()
                    .map_err(|_| err_pair("internal", "lock poisoned"))?;
                match map.get(&name) {
                    Some(r) => Ok(serde_json::to_value(r.summary(&name, now_unix()))
                        .unwrap_or(Value::Null)),
                    None => Err(("not_found".into(), format!("no token named `{name}`"))),
                }
            }
            "delete" => {
                let name = str_field(req, "name")?;
                let removed = crate::lifecycle::delete_locked(self, &name)?;
                Ok(json!({ "deleted": removed }))
            }
            "reveal" => {
                let name = str_field(req, "name")?;
                let map =
                    self.tokens.lock().map_err(|_| err_pair("internal", "lock poisoned"))?;
                let value = crate::lifecycle::usable_value(&map, &name)?;
                Ok(json!({ "value": value }))
            }
            "inject" => self.inject(req),
            "request" => self.http_request(req),
            "refresh" => crate::lifecycle::refresh(self, req),
            "revoke" => crate::lifecycle::revoke(self, req),
            "shutdown" => {
                self.shutdown.store(true, Ordering::SeqCst);
                Ok(json!({ "stopping": true }))
            }
            _ => Err(("unknown_op".into(), format!("unknown vault op `{op}`"))),
        }
    }

    /// Spawn a child with the token as an env var of the child only.
    fn inject(&self, req: &Value) -> Result<Value, (String, String)> {
        let name = str_field(req, "name")?;
        let env_var = str_field(req, "env_var")?;
        let command = str_field(req, "command")?;
        let args = req
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let stdin_text = req.get("stdin_text").and_then(Value::as_str).unwrap_or("");
        let timeout = Duration::from_secs(
            req.get("timeout_secs").and_then(Value::as_u64).unwrap_or(30).max(1),
        );
        let value = {
            let map =
                self.tokens.lock().map_err(|_| err_pair("internal", "lock poisoned"))?;
            crate::lifecycle::usable_value(&map, &name)?
        };
        let out = crate::spawn_capture(&command, &args, &[(env_var, value)], stdin_text, timeout)
            .map_err(|e| ("internal".to_string(), e))?;
        Ok(json!({
            "exit_code": out.exit_code,
            "stdout": trunc(&out.stdout),
            "stderr": trunc(&out.stderr),
        }))
    }

    /// Perform an HTTP call with the token as a Bearer header.
    ///
    /// `method` may be any token-shaped verb (GET/POST plus DAV verbs
    /// such as PROPFIND, REPORT, PUT, DELETE — CalDAV needs those).
    /// The body is either a JSON value (`body`) or a raw string
    /// (`body_text`, e.g. DAV XML or an .ics document); `headers` is an
    /// object of extra header name → value. The token value never
    /// leaves this process: only status + (truncated) body come back.
    fn http_request(&self, req: &Value) -> Result<Value, (String, String)> {
        let name = str_field(req, "name")?;
        let method = str_field(req, "method")?;
        let url = str_field(req, "url")?;
        let body = req.get("body").cloned();
        let body_text = req.get("body_text").and_then(Value::as_str).map(str::to_string);
        // The method is interpolated into the request line: accept only
        // a token-shaped verb so a crafted name cannot smuggle CRLF.
        if method.is_empty()
            || method.len() > 16
            || !method.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(("invalid_args".into(), "method must be a plain HTTP verb".into()));
        }
        let mut extra_headers: Vec<(String, String)> = Vec::new();
        if let Some(h) = req.get("headers").and_then(Value::as_object) {
            for (k, v) in h {
                let v = v.as_str().ok_or_else(|| {
                    ("invalid_args".to_string(), "header values must be strings".to_string())
                })?;
                if k.is_empty()
                    || k.bytes().any(|b| b <= b' ' || b == b':' || b >= 0x7f)
                    || v.bytes().any(|b| b == b'\r' || b == b'\n')
                {
                    return Err((
                        "invalid_args".into(),
                        "invalid header name or value".into(),
                    ));
                }
                extra_headers.push((k.clone(), v.to_string()));
            }
        }
        let value = {
            let map =
                self.tokens.lock().map_err(|_| err_pair("internal", "lock poisoned"))?;
            crate::lifecycle::usable_value(&map, &name)?
        };
        let agent =
            ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build();
        let mut r = agent.request(&method, &url);
        r = r.set("Authorization", &format!("Bearer {value}"));
        for (k, v) in &extra_headers {
            r = r.set(k, v);
        }
        // Drop the token before the network call so it cannot be
        // retained past this request even if the call panics.
        drop(value);
        let resp = match (body, body_text) {
            (Some(b), _) => r.send_json(b),
            (None, Some(t)) => r.send_string(&t),
            (None, None) => r.call(),
        };
        match resp {
            Ok(r) => {
                let status = r.status();
                let text = r.into_string().unwrap_or_default();
                Ok(json!({ "status": status, "body": trunc(&text) }))
            }
            Err(ureq::Error::Status(code, resp)) => {
                // Keep the error body (truncated): CalDAV servers put
                // the reason in an <error> document, not just the code.
                let text = resp.into_string().unwrap_or_default();
                Ok(json!({ "status": code, "body": trunc(&text) }))
            }
            Err(ureq::Error::Transport(_)) => {
                Err(("upstream_error".into(), "could not reach the URL".into()))
            }
        }
    }

    pub fn should_exit(&self) -> bool {
        if self.shutdown.load(Ordering::SeqCst) {
            return true;
        }
        if self.idle_secs() >= self.idle_timeout.as_secs() {
            return true;
        }
        if let Some(pid) = self.parent_pid {
            #[cfg(unix)]
            unsafe {
                // ESRCH = parent is gone.
                if libc::kill(pid, 0) != 0 {
                    return true;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
            }
        }
        false
    }
}

fn err_pair(code: &str, msg: &str) -> (String, String) {
    (code.to_string(), msg.to_string())
}

fn str_field(req: &Value, key: &str) -> Result<String, (String, String)> {
    req.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ("invalid_args".into(), format!("missing string field `{key}`")))
}

