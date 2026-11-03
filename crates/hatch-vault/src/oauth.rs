//! OAuth 2.0 authorization-code flow scaffold — **one flow, mock only**.
//!
//! A local loopback listener (`std::net::TcpListener` on 127.0.0.1, an
//! ephemeral port) receives the redirect, the code is exchanged against a
//! token endpoint, and the resulting token is stored in the vault via the
//! helper. No real provider, no real redirect URI registration: the
//! endpoints in [`OAuthConfig`] are expected to be a local mock server in
//! tests (`hatch-vault/tests` runs one). Against a real provider this
//! scaffold would need a registered client id / redirect URI, which is out
//! of scope for this phase.
//!
//! The caller drives the three steps so tests and the CLI can print the
//! URL, wait for the browser redirect, and exchange the code separately:
//!
//! 1. [`begin_flow`] — bind the loopback listener, build the auth URL.
//! 2. [`PendingFlow::wait_for_code`] — accept one redirect, verify state.
//! 3. [`exchange_code`] — POST the code to the token endpoint, store it.

use hatch_core::{Error, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::client::{StoreRequest, VaultClient};

/// Static configuration for one authorization-code flow.
#[derive(Clone, Debug)]
pub struct OAuthConfig {
    /// Client id shown to the authorization endpoint (mock value in tests).
    pub client_id: String,
    /// Authorization endpoint URL (mock server in tests).
    pub auth_url: String,
    /// Token endpoint URL (mock server in tests).
    pub token_url: String,
    /// Space-separated scopes, if any.
    pub scope: String,
    /// Name under which the resulting token is stored in the vault.
    pub token_name: String,
    /// Connector the token belongs to (recorded on the vault record).
    pub connector: Option<String>,
}

/// A flow that has bound its loopback listener and is waiting for the
/// redirect.
pub struct PendingFlow {
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    config: OAuthConfig,
}

/// Random-ish state value: 16 bytes from /dev/urandom, falling back to a
/// pid + time hash. The state is only a CSRF check for the loopback
/// redirect, not a secret.
fn new_state() -> String {
    let mut buf = [0u8; 16];
    let got = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if got {
        return buf.iter().map(|b| format!("{b:02x}")).collect();
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ ((std::process::id() as u128) << 64);
    format!("{n:032x}")
}

/// Percent-encode a query value (only the small set this flow needs).
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Bind 127.0.0.1 on an ephemeral port and build the authorization URL.
/// Returns the URL (print it / open it) and the pending flow.
pub fn begin_flow(config: OAuthConfig) -> Result<(String, PendingFlow)> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| {
        Error::Connector(format!("oauth: cannot bind loopback listener: {e}"))
    })?;
    let port = listener.local_addr().map_err(Error::from)?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let state = new_state();
    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
        config.auth_url,
        encode(&config.client_id),
        encode(&redirect_uri),
        encode(&config.scope),
        encode(&state),
    );
    Ok((url, PendingFlow { listener, redirect_uri, state, config }))
}

/// Parse `k=v&k=v` query pairs (percent-decoding the small set we emit).
fn query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            let raw = it.next().unwrap_or("");
            let mut out = String::new();
            let mut chars = raw.chars();
            while let Some(c) = chars.next() {
                if c == '%' {
                    let h: String = chars.by_ref().take(2).collect();
                    if let Ok(b) = u8::from_str_radix(&h, 16) {
                        out.push(b as char);
                    }
                } else if c == '+' {
                    out.push(' ');
                } else {
                    out.push(c);
                }
            }
            return Some(out);
        }
    }
    None
}

impl PendingFlow {
    /// The redirect URI the authorization server must return to.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The config this flow was started with.
    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    /// Wait for the single browser redirect (up to `timeout`), verify the
    /// state, and return the authorization code. Replies to the browser
    /// with a plain page in both cases.
    pub fn wait_for_code(self, timeout: Duration) -> Result<String> {
        self.listener.set_nonblocking(false).map_err(Error::from)?;
        let stream = accept_with_timeout(&self.listener, timeout)?;
        handle_redirect(stream, &self.state)
    }
}

fn accept_with_timeout(listener: &TcpListener, timeout: Duration) -> Result<TcpStream> {
    listener.set_nonblocking(true).map_err(Error::from)?;
    let start = std::time::Instant::now();
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false).map_err(Error::from)?;
                return Ok(s);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() >= timeout {
                    return Err(Error::Connector(
                        "oauth: timed out waiting for the redirect".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(Error::from(e)),
        }
    }
}

fn handle_redirect(mut stream: TcpStream, expected_state: &str) -> Result<String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(Error::from)?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).map_err(Error::from)?;
    // Drain headers so the browser's request is fully read.
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(Error::from)?;
        if n == 0 || line.trim().is_empty() {
            break;
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    let query = path.split_once('?').map(|x| x.1).unwrap_or("");
    let reply = |stream: &mut TcpStream, ok: bool, body: &str| {
        let status = if ok { "200 OK" } else { "400 Bad Request" };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.flush();
    };
    match query_value(query, "error") {
        Some(e) => {
            reply(&mut stream, false, &format!("authorization failed: {e}"));
            Err(Error::Connector(format!("oauth: authorization failed: {e}")))
        }
        None => {
            let state = query_value(query, "state").unwrap_or_default();
            if state != expected_state {
                reply(&mut stream, false, "state mismatch");
                return Err(Error::Connector("oauth: state mismatch on redirect".into()));
            }
            match query_value(query, "code") {
                Some(code) if !code.is_empty() => {
                    reply(&mut stream, true, "Authorization received. You can close this tab.");
                    Ok(code)
                }
                _ => {
                    reply(&mut stream, false, "no code in redirect");
                    Err(Error::Connector("oauth: redirect had no code".into()))
                }
            }
        }
    }
}

/// What the token endpoint returned and stored.
#[derive(Clone, Debug)]
pub struct TokenResult {
    pub token_name: String,
    pub expires_at: Option<i64>,
}

/// Exchange `code` at the token endpoint and store the access token in the
/// vault (via the helper). The redirect URI must match the one the flow
/// started with.
pub fn exchange_code(
    config: &OAuthConfig,
    redirect_uri: &str,
    code: &str,
    vault: &VaultClient,
) -> Result<TokenResult> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build();
    let resp = agent
        .post(&config.token_url)
        .send_json(json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect_uri,
            "client_id": config.client_id,
        }))
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => {
                Error::Connector(format!("oauth: token endpoint returned HTTP {code}"))
            }
            ureq::Error::Transport(_) => {
                Error::Connector("oauth: could not reach the token endpoint".into())
            }
        })?;
    let body: Value = resp.into_json().map_err(|_| {
        Error::Connector("oauth: token endpoint returned non-JSON".into())
    })?;
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Connector("oauth: token response has no access_token".into()))?
        .to_string();
    let expires_at = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .map(|secs| crate::record::now_unix() + secs);
    let refresh_token = body.get("refresh_token").and_then(Value::as_str).map(str::to_string);
    vault.store(
        &config.token_name,
        StoreRequest {
            value: access_token,
            connector: config.connector.clone(),
            expires_at,
            refresh_token,
            refresh_supported: false,
            refresh_url: None,
        },
    )?;
    Ok(TokenResult { token_name: config.token_name.clone(), expires_at })
}
