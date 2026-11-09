//! The authorization-code flow with PKCE S256 over a loopback redirect.
//!
//! The caller drives the three steps so the CLI can print the URL, wait
//! for the browser redirect, and exchange the code separately:
//!
//! 1. [`begin_login`] — bind the loopback listener, build the auth URL
//!    (with `code_challenge` + `state`).
//! 2. [`PendingLogin::wait_for_code`](super::loopback::PendingLogin) —
//!    accept one redirect, verify the state, return the code.
//! 3. [`exchange_code`] — POST the code (+ `code_verifier`) to the token
//!    endpoint.
//!
//! Token-endpoint calls are `application/x-www-form-urlencoded` (RFC
//! 6749).

use super::loopback::PendingLogin;
use super::pkce::{challenge_s256, new_state, new_verifier};
use super::store::{OAuthConfig, OAuthTokens};
use hatch_core::{Error, Result};
use serde_json::Value;
use std::net::TcpListener;
use std::time::Duration;

/// Unix seconds now.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Percent-encode a form value (RFC 3986 unreserved set passes through).
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

/// Bind 127.0.0.1 on an ephemeral port and build the authorization URL
/// (PKCE S256 + state). Returns the URL (print it / open it) and the
/// pending login.
pub fn begin_login(config: &OAuthConfig) -> Result<(String, PendingLogin)> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| {
        Error::Connector(format!("oauth: cannot bind loopback listener: {e}"))
    })?;
    let port = listener.local_addr().map_err(Error::from)?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let state = new_state();
    let verifier = new_verifier();
    let challenge = challenge_s256(&verifier);
    let scope = config.scopes.join(" ");
    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        config.authorization_endpoint,
        encode(&config.client_id),
        encode(&redirect_uri),
        encode(&scope),
        encode(&state),
        encode(&challenge),
    );
    Ok((
        url,
        PendingLogin::new(listener, redirect_uri, state, verifier, config.clone()),
    ))
}

/// POST form fields to a token endpoint and parse the token response.
/// `grant_type` + fields are the body; the response must carry an
/// `access_token`. A missing new refresh token keeps `keep_refresh`.
fn post_token(
    token_url: &str,
    fields: &[(&str, &str)],
    keep_refresh: Option<String>,
) -> Result<OAuthTokens> {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let resp = agent
        .post(token_url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => {
                Error::Connector(format!("oauth: token endpoint returned HTTP {code}"))
            }
            ureq::Error::Transport(_) => {
                Error::Connector("oauth: could not reach the token endpoint".into())
            }
        })?;
    let json: Value = resp.into_json().map_err(|_| {
        Error::Connector("oauth: token endpoint returned non-JSON".into())
    })?;
    parse_token_response(&json, keep_refresh)
}

/// Read a token-endpoint JSON body into [`OAuthTokens`].
fn parse_token_response(json: &Value, keep_refresh: Option<String>) -> Result<OAuthTokens> {
    let access_token = json
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Connector("oauth: token response has no access_token".into()))?
        .to_string();
    // A refresh response may omit a new refresh token: the old one
    // stays valid (RFC 6749 §6).
    let refresh_token = json
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(keep_refresh);
    let expires_at = json
        .get("expires_in")
        .and_then(Value::as_i64)
        .map(|secs| now_unix() + secs);
    Ok(OAuthTokens { access_token, refresh_token, expires_at })
}

/// Exchange `code` at the token endpoint (with the PKCE verifier) and
/// return the tokens. The redirect URI must match the one the login
/// started with.
pub fn exchange_code(
    config: &OAuthConfig,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<OAuthTokens> {
    post_token(
        &config.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &config.client_id),
            ("code_verifier", verifier),
        ],
        None,
    )
}

/// Refresh the access token with a refresh token. Returns the new
/// tokens (old refresh token kept when the server issues no new one).
pub fn refresh(config: &OAuthConfig, refresh_token: &str) -> Result<OAuthTokens> {
    post_token(
        &config.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &config.client_id),
        ],
        Some(refresh_token.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn test_config(token_url: &str) -> OAuthConfig {
        OAuthConfig {
            server_url: "http://127.0.0.1:9/mcp".into(),
            client_id: "hatch".into(),
            scopes: vec!["mcp:tools".into()],
            authorization_endpoint: "http://127.0.0.1:9/authorize".into(),
            token_endpoint: token_url.into(),
        }
    }

    /// A one-shot mock token endpoint: answers `body` to the first POST,
    /// counts requests, then closes.
    fn mock_token_endpoint(body: &str, status: u16) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = Arc::clone(&hits);
        let body = body.to_string();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                hits2.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                let mut len = 0usize;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let l = line.trim_end().to_string();
                    if l.is_empty() {
                        break;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0u8; len];
                let _ = reader.read_exact(&mut buf);
                let reason = if status == 200 { "OK" } else { "Bad Request" };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.flush();
            }
        });
        (format!("http://127.0.0.1:{port}/token"), hits)
    }

    #[test]
    fn auth_url_carries_pkce_and_state() {
        let cfg = test_config("http://127.0.0.1:9/token");
        let (url, pending) = begin_login(&cfg).unwrap();
        assert!(url.starts_with("http://127.0.0.1:9/authorize?response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=hatch"));
        assert!(url.contains("scope=mcp%3Atools"));
        // The challenge in the URL is the S256 of the pending verifier.
        let challenge = challenge_s256(pending.verifier());
        assert!(url.contains(&format!("code_challenge={challenge}")));
        assert_ne!(pending.verifier(), challenge);
        drop(pending);
    }

    #[test]
    fn exchange_posts_form_with_verifier_and_parses_tokens() {
        let (url, hits) = mock_token_endpoint(
            r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#,
            200,
        );
        let cfg = test_config(&url);
        let t = exchange_code(&cfg, "http://127.0.0.1:1/callback", "code-1", "verifier-abc").unwrap();
        assert_eq!(t.access_token, "at-1");
        assert_eq!(t.refresh_token.as_deref(), Some("rt-1"));
        assert!(t.expires_at.unwrap() > now_unix());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn refresh_keeps_old_refresh_token_when_server_omits_a_new_one() {
        let (url, _) = mock_token_endpoint(r#"{"access_token":"at-2","expires_in":60}"#, 200);
        let cfg = test_config(&url);
        let t = refresh(&cfg, "rt-old").unwrap();
        assert_eq!(t.access_token, "at-2");
        assert_eq!(t.refresh_token.as_deref(), Some("rt-old"));
    }

    #[test]
    fn token_endpoint_error_is_a_clear_error_not_a_token() {
        let (url, _) = mock_token_endpoint(r#"{"error":"invalid_grant"}"#, 400);
        let cfg = test_config(&url);
        let err = refresh(&cfg, "rt-dead").unwrap_err();
        assert!(err.to_string().contains("HTTP 400"), "must surface the status: {err}");
    }
}
