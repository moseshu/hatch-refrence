//! The loopback redirect receiver: a `std::net::TcpListener` on
//! 127.0.0.1 (ephemeral port) that accepts exactly one browser
//! redirect, verifies the `state`, and returns the authorization code.
//!
//! Same pattern as the vault OAuth scaffold, which this module does
//! not depend on (no PKCE there). The listener is closed when
//! [`PendingLogin::wait_for_code`] returns, success or failure.

use super::store::OAuthConfig;
use hatch_core::{Error, Result};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// A login that has bound its loopback listener and is waiting for the
/// redirect.
pub struct PendingLogin {
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    verifier: String,
    config: OAuthConfig,
}

impl PendingLogin {
    pub(crate) fn new(
        listener: TcpListener,
        redirect_uri: String,
        state: String,
        verifier: String,
        config: OAuthConfig,
    ) -> Self {
        PendingLogin { listener, redirect_uri, state, verifier, config }
    }

    /// The redirect URI the authorization server must return to.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The config this login started with.
    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    /// Wait for the single browser redirect (up to `timeout`), verify the
    /// state, and return the authorization code. Replies to the browser
    /// with a plain page in both cases. The listener is closed when this
    /// returns, success or failure.
    pub fn wait_for_code(self, timeout: Duration) -> Result<String> {
        let stream = accept_with_timeout(&self.listener, timeout)?;
        handle_redirect(stream, &self.state)
    }

    /// The PKCE verifier for the code exchange (kept with the pending
    /// login so the caller never handles it separately).
    pub fn verifier(&self) -> &str {
        &self.verifier
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

#[cfg(test)]
mod tests {
    use super::super::flow::begin_login;
    use super::super::store::OAuthConfig;
    use std::time::Duration;

    fn test_config() -> OAuthConfig {
        OAuthConfig {
            server_url: "http://127.0.0.1:9/mcp".into(),
            client_id: "hatch".into(),
            scopes: vec!["mcp:tools".into()],
            authorization_endpoint: "http://127.0.0.1:9/authorize".into(),
            token_endpoint: "http://127.0.0.1:9/token".into(),
        }
    }

    fn state_of(url: &str) -> String {
        url.split("state=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    fn wait_for_code_accepts_matching_state_and_rejects_mismatch() {
        let (url, pending) = begin_login(&test_config()).unwrap();
        let redirect = pending.redirect_uri().to_string();
        let state = state_of(&url);
        let good = std::thread::spawn(move || {
            pending.wait_for_code(Duration::from_secs(10)).unwrap()
        });
        // Simulate the authorization server redirecting the browser.
        let target = format!("{redirect}?code=auth-code-1&state={state}");
        let resp = ureq::get(&target).call();
        assert!(resp.is_ok(), "redirect page should answer 200: {resp:?}");
        assert_eq!(good.join().unwrap(), "auth-code-1");

        // Wrong state: the login refuses the code.
        let (_, pending2) = begin_login(&test_config()).unwrap();
        let redirect2 = pending2.redirect_uri().to_string();
        let bad = std::thread::spawn(move || pending2.wait_for_code(Duration::from_secs(10)));
        let target2 = format!("{redirect2}?code=auth-code-2&state=wrong-state");
        let _ = ureq::get(&target2).call();
        assert!(
            bad.join().unwrap().is_err(),
            "a redirect with the wrong state must be rejected"
        );
    }

    #[test]
    fn wait_for_code_times_out_and_closes() {
        let (_, pending) = begin_login(&test_config()).unwrap();
        let start = std::time::Instant::now();
        let err = pending.wait_for_code(Duration::from_millis(200)).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(5), "timeout must not hang");
        assert!(err.to_string().contains("timed out"), "error must say timed out: {err}");
    }
}
