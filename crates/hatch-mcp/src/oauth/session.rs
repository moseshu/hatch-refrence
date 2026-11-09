//! One logged-in MCP server: the config + tokens on disk, the tokens
//! in memory, and the single place refreshes happen.
//!
//! Refresh deduplication: `access_token()` holds one mutex across the
//! freshness check and any refresh. Concurrent callers serialize on
//! that mutex; a caller that waited re-checks freshness first, so one
//! expired token causes exactly one refresh request no matter how many
//! threads asked at once.
//!
//! A failed refresh is a plain error — the caller must not keep using
//! the old token. When there is no refresh token at all the error says
//! to run the login command.

use super::discovery::discover;
use super::flow::{begin_login, exchange_code, now_unix, refresh};
use super::loopback::PendingLogin;
use super::store::{OAuthClientConfig, OAuthConfig, OAuthStore, OAuthTokens};
use hatch_core::{Error, Result};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// How early (seconds) an access token counts as expired.
const REFRESH_MARGIN_SECS: i64 = 60;

/// How long the login command waits for the browser redirect.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);

/// A live OAuth session for one MCP server.
pub struct OAuthSession {
    /// Server name (for error messages and the login hint).
    name: String,
    config: OAuthConfig,
    store: OAuthStore,
    /// Tokens in memory, guarded by the same mutex that serializes
    /// refreshes (see the module docs).
    tokens: Mutex<OAuthTokens>,
}

impl OAuthSession {
    /// Open the session for `server`: load the cached config (discovered
    /// endpoints) or discover them from the server URL, then load any
    /// stored tokens. Discovery runs only when no config is cached.
    pub fn open(
        home: &Path,
        server: &str,
        client: &OAuthClientConfig,
        server_url: &str,
    ) -> Result<Self> {
        let store = OAuthStore::new(home, server)?;
        let config = match store.load_config()? {
            Some(c) => c,
            None => {
                let meta = discover(server_url)?;
                let c = OAuthConfig {
                    server_url: server_url.to_string(),
                    client_id: client.client_id.clone(),
                    scopes: client.scopes.clone(),
                    authorization_endpoint: meta.authorization_endpoint,
                    token_endpoint: meta.token_endpoint,
                };
                store.save_config(&c)?;
                c
            }
        };
        let tokens = store.load_tokens()?.unwrap_or_default();
        Ok(OAuthSession { name: server.to_string(), config, store, tokens: Mutex::new(tokens) })
    }

    /// The server name this session is for.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A usable access token: the stored one while fresh, otherwise a
    /// refresh (deduplicated across threads). Errors when there is no
    /// token and no refresh token — the caller must run the login flow.
    pub fn access_token(&self) -> Result<String> {
        let mut guard = self.tokens.lock().map_err(|_| {
            Error::Connector("oauth: token lock poisoned".into())
        })?;
        if guard.is_fresh(now_unix(), REFRESH_MARGIN_SECS) {
            return Ok(guard.access_token.clone());
        }
        let Some(rt) = guard.refresh_token.clone() else {
            return Err(Error::Connector(format!(
                "mcp server {}: no OAuth token — run `hatch mcp login {}` first",
                self.name, self.name
            )));
        };
        let fresh = refresh(&self.config, &rt).map_err(|e| {
            Error::Connector(format!(
                "mcp server {}: token refresh failed ({e}) — run `hatch mcp login {}` to log in again",
                self.name, self.name
            ))
        })?;
        *guard = fresh.clone();
        self.store.save_tokens(&fresh)?;
        Ok(fresh.access_token)
    }

    /// Force a refresh after a 401: the token looked fresh by expiry
    /// but the server rejected it (revoked, or clock skew). `stale` is
    /// the token that got the 401 — when another thread already
    /// refreshed past it, the new token is returned without a second
    /// network call. A failed refresh is an error, never a silent
    /// retry with the dead token.
    pub fn refresh_after_401(&self, stale: &str) -> Result<String> {
        let mut guard = self.tokens.lock().map_err(|_| {
            Error::Connector("oauth: token lock poisoned".into())
        })?;
        if guard.access_token != stale && guard.is_fresh(now_unix(), REFRESH_MARGIN_SECS) {
            return Ok(guard.access_token.clone());
        }
        let Some(rt) = guard.refresh_token.clone() else {
            return Err(Error::Connector(format!(
                "mcp server {}: token rejected and no refresh token — run `hatch mcp login {}` to log in again",
                self.name, self.name
            )));
        };
        let fresh = refresh(&self.config, &rt).map_err(|e| {
            Error::Connector(format!(
                "mcp server {}: token refresh failed ({e}) — run `hatch mcp login {}` to log in again",
                self.name, self.name
            ))
        })?;
        *guard = fresh.clone();
        self.store.save_tokens(&fresh)?;
        Ok(fresh.access_token)
    }

    /// Start the interactive login: returns the authorization URL to
    /// open in a browser and the pending login. The URL is printed by
    /// the caller — never logged with any token in it (it carries only
    /// the challenge and state, which are single-use).
    pub fn begin_login(&self) -> Result<(String, PendingLogin)> {
        begin_login(&self.config)
    }

    /// Finish a login: wait for the redirect code, exchange it, and
    /// store the tokens.
    pub fn finish_login(&self, pending: PendingLogin) -> Result<()> {
        let redirect_uri = pending.redirect_uri().to_string();
        let verifier = pending.verifier().to_string();
        let config = pending.config().clone();
        let code = pending.wait_for_code(LOGIN_TIMEOUT)?;
        let tokens = exchange_code(&config, &redirect_uri, &code, &verifier)?;
        *self.tokens.lock().map_err(|_| {
            Error::Connector("oauth: token lock poisoned".into())
        })? = tokens.clone();
        self.store.save_tokens(&tokens)?;
        Ok(())
    }

    /// Forget the stored tokens (logout). The config stays.
    pub fn logout(&self) -> Result<()> {
        *self.tokens.lock().map_err(|_| {
            Error::Connector("oauth: token lock poisoned".into())
        })? = OAuthTokens::default();
        self.store.clear_tokens()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A mock token endpoint that answers `body` to every POST and
    /// counts the calls. Runs on a detached thread until the test
    /// process exits.
    fn mock_token_server(body: &str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = Arc::clone(&hits);
        let body = body.to_string();
        std::thread::spawn(move || loop {
            let mut stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(_) => return,
            };
            hits2.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
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
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
        });
        (format!("http://127.0.0.1:{port}/token"), hits)
    }

    fn session_with_tokens(home: &Path, tokens: OAuthTokens, token_url: &str) -> OAuthSession {
        let store = OAuthStore::new(home, "srv").unwrap();
        store
            .save_config(&OAuthConfig {
                server_url: "http://127.0.0.1:9/mcp".into(),
                client_id: "hatch".into(),
                scopes: vec![],
                authorization_endpoint: "http://127.0.0.1:9/authorize".into(),
                token_endpoint: token_url.into(),
            })
            .unwrap();
        store.save_tokens(&tokens).unwrap();
        OAuthSession::open(
            home,
            "srv",
            &OAuthClientConfig { client_id: "hatch".into(), scopes: vec![] },
            "http://127.0.0.1:9/mcp",
        )
        .unwrap()
    }

    #[test]
    fn concurrent_refresh_posts_exactly_once() {
        let home = tempfile::tempdir().unwrap();
        let (url, hits) = mock_token_server(
            r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":3600}"#,
        );
        let expired = OAuthTokens {
            access_token: "at-old".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: Some(now_unix() - 10),
        };
        let session = Arc::new(session_with_tokens(home.path(), expired, &url));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = Arc::clone(&session);
            handles.push(std::thread::spawn(move || s.access_token().unwrap()));
        }
        let mut got = Vec::new();
        for h in handles {
            got.push(h.join().unwrap());
        }
        assert!(got.iter().all(|t| t == "at-new"));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "8 concurrent callers must cause exactly one refresh POST"
        );
        // The new tokens were persisted.
        let stored = OAuthStore::new(home.path(), "srv").unwrap().load_tokens().unwrap().unwrap();
        assert_eq!(stored.access_token, "at-new");
        assert_eq!(stored.refresh_token.as_deref(), Some("rt-new"));
    }

    #[test]
    fn fresh_token_needs_no_network() {
        let home = tempfile::tempdir().unwrap();
        let (url, hits) =
            mock_token_server(r#"{"access_token":"at-new","expires_in":3600}"#);
        let fresh = OAuthTokens {
            access_token: "at-fresh".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: Some(now_unix() + 3600),
        };
        let session = session_with_tokens(home.path(), fresh, &url);
        assert_eq!(session.access_token().unwrap(), "at-fresh");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn no_tokens_is_a_login_error_not_a_silent_empty_token() {
        let home = tempfile::tempdir().unwrap();
        let (url, _) = mock_token_server(r#"{"access_token":"x"}"#);
        let session = session_with_tokens(home.path(), OAuthTokens::default(), &url);
        let err = session.access_token().unwrap_err();
        assert!(err.to_string().contains("hatch mcp login srv"), "must point at login: {err}");
    }

    #[test]
    fn failed_refresh_is_an_error_and_keeps_nothing_new() {
        let home = tempfile::tempdir().unwrap();
        // The mock answers 200 with no access_token: parse fails.
        let (url, _) = mock_token_server(r#"{"error":"invalid_grant"}"#);
        let expired = OAuthTokens {
            access_token: "at-old".into(),
            refresh_token: Some("rt-dead".into()),
            expires_at: Some(now_unix() - 10),
        };
        let session = session_with_tokens(home.path(), expired, &url);
        let err = session.access_token().unwrap_err();
        assert!(err.to_string().contains("refresh failed"), "must say refresh failed: {err}");
        let stored = OAuthStore::new(home.path(), "srv").unwrap().load_tokens().unwrap().unwrap();
        assert_eq!(stored.access_token, "at-old", "failed refresh must not overwrite stored tokens");
    }
}
