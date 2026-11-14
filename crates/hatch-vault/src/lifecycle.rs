//! OAuth token lifecycle: validated refresh, revocation, and the
//! revoked-token block (task 8, §3.3).
//!
//! Refresh rules (§3.3 "刷新时校验供应商地址、账户、范围及并发轮换"):
//!
//! * the stored refresh URL's host must match the provider host recorded
//!   when the token was stored — a tampered `tokens.json` cannot redirect
//!   a refresh to an attacker's host;
//! * when the caller names an expected account, it must match the account
//!   recorded on the token;
//! * a scope the provider returns must be a subset of the granted scope
//!   (refresh may narrow, never widen);
//! * concurrent rotation is serialized with an `flock` on
//!   `tokens.json.lock`: each refresh re-reads the file under the lock,
//!   and if another helper rotated the token while this one was talking
//!   to the provider, this one's fresh tokens are discarded instead of
//!   clobbering the winner's (compare-and-swap on `generation` +
//!   `refresh_token`).
//!
//! Revoke rules (§3.3 "撤销后阻断新的请求并使相关审批重新核对"):
//!
//! * revoke clears the secret material from the file, marks the record
//!   revoked, and every helper op (`inject` / `request` / `reveal` /
//!   `refresh`) plus the legacy file path refuse a revoked token;
//! * the provider revocation endpoint is attempted when recorded, but a
//!   failure there never un-blocks hatch: the local block is the safety
//!   property, and the response says whether the provider was told;
//! * the CLI lists pending approvals naming the connector, so the user
//!   can re-check them; the connector executor refuses a revoked token
//!   at execution time (see `hatch-tools/src/connector.rs`).
//!
//! All of this is mechanism against a **mock** provider (local test
//! server in the tests below, `hatch-vault/tests` style). No real
//! provider, no real account: wiring a real provider needs a registered
//! client id / redirect URI, which is out of scope for this phase.

use crate::helper::Helper;
use crate::record::{now_unix, TokenRecord};
use hatch_core::time::to_rfc3339;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

type HResult<T> = Result<T, (String, String)>;

fn err_pair(code: &str, msg: String) -> (String, String) {
    (code.to_string(), msg)
}

/// Host part of a URL (`http://127.0.0.1:8080/token` → `127.0.0.1`),
/// lowercased. Hand-rolled: this crate has no URL parser, and refresh
/// validation only needs the host, not a full parse.
pub fn url_host(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let hostport = after_scheme.split('/').next().unwrap_or("");
    let host = hostport.split(':').next().unwrap_or("");
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// True when every scope in `returned` (space-separated) is in
/// `granted`. `None` on either side means "no scope recorded": nothing
/// to check, so it passes.
pub fn scope_subset(granted: Option<&str>, returned: Option<&str>) -> bool {
    let Some(g) = granted else { return true };
    let Some(r) = returned else { return true };
    let have: Vec<&str> = g.split_whitespace().collect();
    r.split_whitespace().all(|s| have.contains(&s))
}

/// Exclusive lock on the token store, for cross-process rotation
/// safety. Unix: `flock` on `tokens.json.lock` (blocking). Other
/// platforms: a process-wide mutex — correct in-process, best effort
/// across processes (the helper only runs on unix in practice).
pub struct FileLock {
    #[cfg(unix)]
    #[allow(dead_code)] // held only to keep the fd (and the flock) alive
    file: std::fs::File,
    #[cfg(not(unix))]
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl FileLock {
    pub fn acquire(lock_path: &Path) -> std::io::Result<FileLock> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(lock_path)?;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(FileLock { file })
        }
        #[cfg(not(unix))]
        {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let guard = LOCK.lock().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::Other, "lock poisoned")
            })?;
            let _ = lock_path;
            Ok(FileLock { _guard: guard })
        }
    }
}

fn lock_path(store_path: &Path) -> PathBuf {
    store_path.with_file_name("tokens.json.lock")
}

/// The value of a usable token, or why it cannot be used. Every op
/// that hands a token out goes through here.
pub fn usable_value(
    map: &BTreeMap<String, TokenRecord>,
    name: &str,
) -> HResult<String> {
    match map.get(name) {
        None => Err(err_pair("not_found", format!("no token named `{name}`"))),
        Some(r) if r.revoked => Err(err_pair(
            "revoked",
            format!("token `{name}` was revoked; re-authorize before use"),
        )),
        Some(r) => Ok(r.value.clone()),
    }
}

/// Store one record under the store lock (re-read first, so a
/// concurrent rotation is never clobbered).
pub(crate) fn store_locked(helper: &Helper, name: &str, rec: TokenRecord) -> HResult<()> {
    let _lock = FileLock::acquire(&lock_path(helper.store_path()))
        .map_err(|e| err_pair("internal", format!("could not lock token store: {e}")))?;
    helper
        .reload()
        .map_err(|e| err_pair("internal", e))?;
    {
        let mut map = helper
            .tokens()
            .lock()
            .map_err(|_| err_pair("internal", "lock poisoned".to_string()))?;
        map.insert(name.to_string(), rec);
    }
    helper.save().map_err(|e| err_pair("internal", e))?;
    Ok(())
}

/// Delete one record under the store lock.
pub(crate) fn delete_locked(helper: &Helper, name: &str) -> HResult<bool> {
    let _lock = FileLock::acquire(&lock_path(helper.store_path()))
        .map_err(|e| err_pair("internal", format!("could not lock token store: {e}")))?;
    helper
        .reload()
        .map_err(|e| err_pair("internal", e))?;
    let removed = {
        let mut map = helper
            .tokens()
            .lock()
            .map_err(|_| err_pair("internal", "lock poisoned".to_string()))?;
        map.remove(name).is_some()
    };
    if removed {
        helper.save().map_err(|e| err_pair("internal", e))?;
    }
    Ok(removed)
}

enum RefreshOutcome {
    Done(Value),
    /// Another helper rotated the token while this one was talking to
    /// the provider: discard the fresh tokens, re-read, retry once.
    RotatedElsewhere,
}

/// Refresh one token, with provider/account/scope validation and
/// locked concurrent rotation. `req` may carry `account` and `scope`
/// expectations; both are checked when the record has them recorded.
pub(crate) fn refresh(helper: &Helper, req: &Value) -> HResult<Value> {
    let name = req
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err_pair("invalid_args", "missing string field `name`".into()))?
        .to_string();
    let expected_account = req.get("account").and_then(Value::as_str);
    let expected_scope = req.get("scope").and_then(Value::as_str);
    for _ in 0..2 {
        match refresh_once(helper, &name, expected_account, expected_scope)? {
            RefreshOutcome::Done(v) => return Ok(v),
            RefreshOutcome::RotatedElsewhere => continue,
        }
    }
    Err(err_pair(
        "concurrent_refresh",
        "another refresh rotated the token; retry".into(),
    ))
}

fn refresh_once(
    helper: &Helper,
    name: &str,
    expected_account: Option<&str>,
    expected_scope: Option<&str>,
) -> HResult<RefreshOutcome> {
    let _lock = FileLock::acquire(&lock_path(helper.store_path()))
        .map_err(|e| err_pair("internal", format!("could not lock token store: {e}")))?;
    helper
        .reload()
        .map_err(|e| err_pair("internal", e))?;
    let snap = {
        let map = helper
            .tokens()
            .lock()
            .map_err(|_| err_pair("internal", "lock poisoned".to_string()))?;
        match map.get(name) {
            None => return Err(err_pair("not_found", format!("no token named `{name}`"))),
            Some(r) => r.clone(),
        }
    };
    if snap.revoked {
        return Err(err_pair(
            "revoked",
            format!("token `{name}` was revoked; re-authorize before use"),
        ));
    }
    let (refresh_token, refresh_url) = match (snap.refresh_supported, &snap.refresh_token, &snap.refresh_url) {
        (true, Some(t), Some(u)) => (t.clone(), u.clone()),
        _ => {
            return Err(err_pair(
                "refresh_unsupported",
                "refresh is not supported for this connector".into(),
            ))
        }
    };
    // Provider host: the refresh URL must talk to the provider recorded
    // at store time. A record without a provider URL is a legacy record:
    // the stored refresh URL is trusted as-is.
    if let Some(provider) = &snap.provider_url {
        let want = url_host(provider);
        let got = url_host(&refresh_url);
        if want != got {
            return Err(err_pair(
                "refresh_provider_mismatch",
                format!("refresh URL host {got:?} does not match the recorded provider host {want:?}; refusing to send the refresh token"),
            ));
        }
    }
    if let (Some(want), Some(have)) = (expected_account, &snap.account) {
        if want != have {
            return Err(err_pair(
                "refresh_account_mismatch",
                format!("refresh for account `{want}` but the token belongs to `{have}`"),
            ));
        }
    }
    if let Some(want) = expected_scope {
        if !scope_subset(snap.scope.as_deref(), Some(want)) {
            return Err(err_pair(
                "refresh_scope_mismatch",
                "requested scope is not covered by the granted scope".into(),
            ));
        }
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let resp = agent.post(&refresh_url).send_json(json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    }));
    let body: Value = match resp {
        Ok(r) => r.into_json().map_err(|_| {
            err_pair("upstream_error", "refresh endpoint returned non-JSON".into())
        })?,
        Err(ureq::Error::Status(code, _)) => {
            return Err(err_pair(
                "upstream_error",
                format!("refresh endpoint returned HTTP {code}"),
            ))
        }
        Err(ureq::Error::Transport(_)) => {
            return Err(err_pair(
                "upstream_error",
                "could not reach refresh endpoint".into(),
            ))
        }
    };
    let new_value = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            err_pair("upstream_error", "refresh response has no access_token".into())
        })?
        .to_string();
    let returned_scope = body.get("scope").and_then(Value::as_str);
    if !scope_subset(snap.scope.as_deref(), returned_scope) {
        return Err(err_pair(
            "refresh_scope_mismatch",
            "provider returned a scope wider than the granted scope; refusing the tokens".into(),
        ));
    }
    // Compare-and-swap: another helper may have rotated the token while
    // this POST was in flight. Its tokens win; ours are discarded.
    helper
        .reload()
        .map_err(|e| err_pair("internal", e))?;
    let changed = {
        let map = helper
            .tokens()
            .lock()
            .map_err(|_| err_pair("internal", "lock poisoned".to_string()))?;
        match map.get(name) {
            Some(cur) => {
                cur.generation != snap.generation || cur.refresh_token != snap.refresh_token
            }
            None => true,
        }
    };
    if changed {
        return Ok(RefreshOutcome::RotatedElsewhere);
    }
    let mut rec = snap.clone();
    rec.value = new_value;
    if let Some(secs) = body.get("expires_in").and_then(Value::as_i64) {
        rec.expires_at = Some(now_unix() + secs);
    }
    if let Some(rt) = body.get("refresh_token").and_then(Value::as_str) {
        rec.refresh_token = Some(rt.to_string());
    }
    if let Some(s) = returned_scope {
        rec.scope = Some(s.to_string());
    }
    rec.generation += 1;
    {
        let mut map = helper
            .tokens()
            .lock()
            .map_err(|_| err_pair("internal", "lock poisoned".to_string()))?;
        map.insert(name.to_string(), rec);
    }
    helper.save().map_err(|e| err_pair("internal", e))?;
    Ok(RefreshOutcome::Done(json!({
        "refreshed": name,
        "generation": snap.generation + 1,
    })))
}

/// Revoke one token. The provider endpoint is attempted when recorded,
/// but a failure there never un-blocks hatch: the record is cleared and
/// marked revoked either way, and the response says whether the
/// provider was told.
pub(crate) fn revoke(helper: &Helper, req: &Value) -> HResult<Value> {
    let name = req
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err_pair("invalid_args", "missing string field `name`".into()))?
        .to_string();
    let _lock = FileLock::acquire(&lock_path(helper.store_path()))
        .map_err(|e| err_pair("internal", format!("could not lock token store: {e}")))?;
    helper
        .reload()
        .map_err(|e| err_pair("internal", e))?;
    let snap = {
        let map = helper
            .tokens()
            .lock()
            .map_err(|_| err_pair("internal", "lock poisoned".to_string()))?;
        match map.get(&name) {
            None => return Err(err_pair("not_found", format!("no token named `{name}`"))),
            Some(r) => r.clone(),
        }
    };
    if snap.revoked {
        return Ok(json!({ "revoked": name, "already": true }));
    }
    let mut provider_revoked = false;
    let mut provider_error: Option<String> = None;
    if let Some(url) = &snap.revoke_url {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build();
        match agent.post(url).send_json(json!({
            "token": snap.value,
            "token_type_hint": "access_token",
        })) {
            Ok(r) if r.status() < 400 => provider_revoked = true,
            Ok(r) => {
                provider_error = Some(format!("revocation endpoint returned HTTP {}", r.status()))
            }
            Err(e) => provider_error = Some(format!("could not reach revocation endpoint: {e}")),
        }
    }
    let mut rec = snap.clone();
    rec.value = String::new();
    rec.refresh_token = None;
    rec.revoked = true;
    rec.revoked_at = Some(to_rfc3339(&hatch_core::time::now()));
    rec.generation += 1;
    {
        let mut map = helper
            .tokens()
            .lock()
            .map_err(|_| err_pair("internal", "lock poisoned".to_string()))?;
        map.insert(name.clone(), rec);
    }
    helper.save().map_err(|e| err_pair("internal", e))?;
    let mut out = json!({ "revoked": name, "provider_revoked": provider_revoked });
    if let Some(e) = provider_error {
        out["provider_error"] = json!(e);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::Helper;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
    }

    /// Tiny mock token server: POST /token rotates, POST /revoke records.
    /// A refresh token the server has already rotated is rejected, the
    /// way a real provider rejects a replayed refresh token.
    struct MockServer {
        base: String,
        rotations: Arc<AtomicUsize>,
        rejected: Arc<AtomicUsize>,
        revoked_calls: Arc<AtomicUsize>,
    }

    impl MockServer {
        fn start(returned_scope: Option<String>) -> (MockServer, std::thread::JoinHandle<()>) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let valid_refresh = Arc::new(Mutex::new("refresh-0".to_string()));
            let rotations = Arc::new(AtomicUsize::new(0));
            let rejected = Arc::new(AtomicUsize::new(0));
            let revoked_calls = Arc::new(AtomicUsize::new(0));
            let srv = MockServer {
                base: format!("http://127.0.0.1:{port}"),
                rotations: rotations.clone(),
                rejected: rejected.clone(),
                revoked_calls: revoked_calls.clone(),
            };
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming().take(64) {
                    let Ok(mut s) = stream else { continue };
                    // Read until the headers end and the Content-Length
                    // body is complete: ureq may split them across
                    // segments, and a single read can stop mid-request.
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 1024];
                    let mut content_len = 0usize;
                    let mut header_end = None;
                    while let Ok(n) = s.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        raw.extend_from_slice(&buf[..n]);
                        if header_end.is_none() {
                            if let Some(pos) = find_subslice(&raw, b"\r\n\r\n") {
                                header_end = Some(pos + 4);
                                let head = String::from_utf8_lossy(&raw[..pos]).into_owned();
                                for line in head.lines().skip(1) {
                                    if let Some(v) = line
                                        .strip_prefix("Content-Length:")
                                        .or_else(|| line.strip_prefix("content-length:"))
                                    {
                                        content_len = v.trim().parse().unwrap_or(0);
                                    }
                                }
                            }
                        }
                        if let Some(end) = header_end {
                            if raw.len() >= end + content_len {
                                break;
                            }
                        }
                        if raw.len() > 65536 {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    let path = text
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let body = match header_end {
                        Some(end) => text[end..].to_string(),
                        None => String::new(),
                    };
                    let mut reply = |status: &str, json: &str| {
                        let _ = write!(
                            s,
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                            json.len()
                        );
                        let _ = s.flush();
                    };
                    if path == "/revoke" {
                        revoked_calls.fetch_add(1, Ordering::SeqCst);
                        reply("200 OK", "{}");
                        continue;
                    }
                    if path != "/token" {
                        reply("404 Not Found", "{}");
                        continue;
                    }
                    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let presented = v
                        .get("refresh_token")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let mut valid = valid_refresh.lock().unwrap();
                    if *valid != presented {
                        rejected.fetch_add(1, Ordering::SeqCst);
                        drop(valid);
                        reply("400 Bad Request", "{\"error\":\"invalid_grant\"}");
                        continue;
                    }
                    let n = rotations.fetch_add(1, Ordering::SeqCst) + 1;
                    let next = format!("refresh-{n}");
                    *valid = next.clone();
                    drop(valid);
                    let scope = match &returned_scope {
                        Some(s) => format!(",\"scope\":\"{s}\""),
                        None => String::new(),
                    };
                    reply(
                        "200 OK",
                        &format!(
                            "{{\"access_token\":\"access-{n}\",\"refresh_token\":\"{next}\",\"expires_in\":3600{scope}}}"
                        ),
                    );
                }
            });
            (srv, handle)
        }
    }

    fn helper_on(home: &Path) -> Helper {
        // `open` creates the vault dir (0700); the file lock lives in it.
        let _ = crate::open(home);
        Helper::load(home, 300, None)
    }

    fn seed(helper: &Helper, server: &MockServer, scope: &str) {
        let mut rec = TokenRecord::new("access-0");
        rec.connector = Some("mock-mail".into());
        rec.account = Some("alice@example.test".into());
        rec.scope = Some(scope.into());
        rec.provider_url = Some(format!("{}/token", server.base));
        rec.revoke_url = Some(format!("{}/revoke", server.base));
        rec.refresh_token = Some("refresh-0".into());
        rec.refresh_supported = true;
        rec.refresh_url = Some(format!("{}/token", server.base));
        store_locked(helper, "mock", rec).unwrap();
    }

    #[test]
    fn refresh_rotates_and_bumps_generation() {
        let home = tempfile::tempdir().unwrap();
        let (server, _h) = MockServer::start(None);
        let helper = helper_on(home.path());
        seed(&helper, &server, "mail.read mail.send");
        let out = refresh(&helper, &json!({ "name": "mock" })).unwrap();
        assert_eq!(out["refreshed"], "mock");
        assert_eq!(out["generation"], 1);
        let map = helper.tokens().lock().unwrap();
        let rec = map.get("mock").unwrap();
        assert_eq!(rec.value, "access-1");
        assert_eq!(rec.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(rec.generation, 1);
        assert_eq!(server.rotations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn refresh_rejects_tampered_provider_host() {
        let home = tempfile::tempdir().unwrap();
        let (server, _h) = MockServer::start(None);
        let helper = helper_on(home.path());
        seed(&helper, &server, "mail.read");
        // Tamper with the stored refresh URL: point it at another host.
        {
            let mut map = helper.tokens().lock().unwrap();
            let rec = map.get_mut("mock").unwrap();
            rec.refresh_url = Some("http://192.0.2.99/token".into());
        }
        helper.save().unwrap();
        let err = refresh(&helper, &json!({ "name": "mock" })).unwrap_err();
        assert_eq!(err.0, "refresh_provider_mismatch");
        // The token is untouched.
        let map = helper.tokens().lock().unwrap();
        assert_eq!(map.get("mock").unwrap().value, "access-0");
        assert_eq!(server.rotations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn refresh_rejects_account_and_scope_mismatch() {
        let home = tempfile::tempdir().unwrap();
        let (server, _h) = MockServer::start(None);
        let helper = helper_on(home.path());
        seed(&helper, &server, "mail.read");
        let err = refresh(
            &helper,
            &json!({ "name": "mock", "account": "bob@example.test" }),
        )
        .unwrap_err();
        assert_eq!(err.0, "refresh_account_mismatch");
        let err = refresh(&helper, &json!({ "name": "mock", "scope": "mail.send" })).unwrap_err();
        assert_eq!(err.0, "refresh_scope_mismatch");
        assert_eq!(server.rotations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn refresh_rejects_widened_scope_from_provider() {
        let home = tempfile::tempdir().unwrap();
        let (server, _h) = MockServer::start(Some("mail.read mail.send mail.admin".into()));
        let helper = helper_on(home.path());
        seed(&helper, &server, "mail.read mail.send");
        let err = refresh(&helper, &json!({ "name": "mock" })).unwrap_err();
        assert_eq!(err.0, "refresh_scope_mismatch");
        let map = helper.tokens().lock().unwrap();
        assert_eq!(map.get("mock").unwrap().value, "access-0");
    }

    #[test]
    fn concurrent_refresh_rotates_without_replay() {
        let home = tempfile::tempdir().unwrap();
        let (server, _h) = MockServer::start(None);
        let helper_a = helper_on(home.path());
        seed(&helper_a, &server, "mail.read");
        let helper_b = helper_on(home.path());
        let a = Arc::new(helper_a);
        let b = Arc::new(helper_b);
        let mut threads = Vec::new();
        for i in 0..4 {
            let h = if i % 2 == 0 { a.clone() } else { b.clone() };
            threads.push(std::thread::spawn(move || {
                refresh(&h, &json!({ "name": "mock" })).unwrap()
            }));
        }
        let mut gens: Vec<u64> = threads
            .into_iter()
            .map(|t| t.join().unwrap()["generation"].as_u64().unwrap())
            .collect();
        gens.sort_unstable();
        assert_eq!(gens, vec![1, 2, 3, 4], "each rotation wins once, in order");
        // No thread ever presented a stale refresh token.
        assert_eq!(server.rejected.load(Ordering::SeqCst), 0);
        assert_eq!(server.rotations.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn revoke_blocks_every_use_and_clears_secrets() {
        let home = tempfile::tempdir().unwrap();
        let (server, _h) = MockServer::start(None);
        let helper = helper_on(home.path());
        seed(&helper, &server, "mail.read");
        let out = revoke(&helper, &json!({ "name": "mock" })).unwrap();
        assert_eq!(out["revoked"], "mock");
        assert_eq!(out["provider_revoked"], true);
        assert_eq!(server.revoked_calls.load(Ordering::SeqCst), 1);
        // Every use is refused.
        let map = helper.tokens().lock().unwrap();
        assert!(usable_value(&map, "mock").is_err());
        let err = usable_value(&map, "mock").unwrap_err();
        assert_eq!(err.0, "revoked");
        drop(map);
        // The file no longer holds the secret material.
        let text = std::fs::read_to_string(crate::vault_path(home.path())).unwrap();
        assert!(!text.contains("access-0"), "access token must be gone from the file");
        assert!(!text.contains("refresh-0"), "refresh token must be gone from the file");
        assert!(text.contains("\"revoked\":true"), "revocation is persisted");
        // A second revoke is a no-op success.
        let again = revoke(&helper, &json!({ "name": "mock" })).unwrap();
        assert_eq!(again["already"], true);
        // Refresh after revoke is refused without touching the provider.
        let err = refresh(&helper, &json!({ "name": "mock" })).unwrap_err();
        assert_eq!(err.0, "revoked");
        assert_eq!(server.rotations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn revoke_without_endpoint_still_blocks_locally() {
        let home = tempfile::tempdir().unwrap();
        let (server, _h) = MockServer::start(None);
        let helper = helper_on(home.path());
        seed(&helper, &server, "mail.read");
        {
            let mut map = helper.tokens().lock().unwrap();
            map.get_mut("mock").unwrap().revoke_url = None;
        }
        helper.save().unwrap();
        let out = revoke(&helper, &json!({ "name": "mock" })).unwrap();
        assert_eq!(out["provider_revoked"], false);
        assert_eq!(server.revoked_calls.load(Ordering::SeqCst), 0);
        let map = helper.tokens().lock().unwrap();
        assert!(map.get("mock").unwrap().revoked);
    }

    #[test]
    fn url_host_and_scope_subset() {
        assert_eq!(url_host("http://127.0.0.1:8080/token").as_deref(), Some("127.0.0.1"));
        assert_eq!(url_host("https://Example.TEST/x").as_deref(), Some("example.test"));
        assert_eq!(url_host("not-a-url"), None);
        assert!(scope_subset(Some("a b"), Some("a")));
        assert!(!scope_subset(Some("a"), Some("a b")));
        assert!(scope_subset(None, Some("a b")));
        assert!(scope_subset(Some("a"), None));
    }
}
