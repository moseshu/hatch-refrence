//! One forwarded model call: build the upstream request, send it,
//! retry transport-level failures.
//!
//! Retry policy (centralized here, so every client gets the same
//! behaviour): 429 and 5xx and transport errors are retried with
//! exponential backoff (`base_ms`, doubling, capped at 8 s, default
//! 3 retries). 400 / 401 / 403 are *not* retried — they are the
//! client's problem, and the client (the provider client in
//! `hatch-llm`) already knows what to do with them: the `temperature`
//! 400 retry, for example, still runs there, because the proxy
//! returns the upstream 400 verbatim. A retry only ever happens
//! before the first upstream byte is read, so a retried call can
//! never duplicate output already sent downstream.

use std::io::Read;
use std::time::Duration;

/// Largest upstream error body kept for a verbatim reply (64 KiB).
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Backoff cap (8 s).
const MAX_BACKOFF_MS: u64 = 8000;

/// Whether an upstream status is worth retrying: rate-limited or a
/// server-side failure. Client errors (400/401/403/404/…) are not.
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || status >= 500
}

/// A completed upstream call: status, forwardable headers, and a
/// streaming body reader.
pub struct UpstreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

/// A failed upstream call after retries are exhausted.
pub enum ForwardError {
    /// Upstream answered with a final status: return its body
    /// verbatim so the client sees the real error.
    Upstream { status: u16, body: Vec<u8> },
    /// No response at all (transport failure after retries).
    Transport(String),
}

impl ForwardError {
    pub fn message(&self) -> String {
        match self {
            ForwardError::Upstream { status, .. } => format!("upstream HTTP {status}"),
            ForwardError::Transport(m) => format!("upstream unreachable: {m}"),
        }
    }
}

/// Response headers that must never be forwarded downstream:
/// hop-by-hop framing, plus `content-encoding` (the proxy forces
/// identity encoding upstream so usage extraction sees plain text).
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "transfer-encoding"
            | "content-length"
            | "keep-alive"
            | "trailer"
            | "upgrade"
            | "proxy-connection"
            | "te"
            | "content-encoding"
    )
}

/// Backoff before attempt `n` (0-based): base × 2ⁿ, capped at 8 s.
fn backoff_ms(base_ms: u64, attempt: u32) -> u64 {
    base_ms
        .saturating_mul(1u64 << attempt.min(10))
        .min(MAX_BACKOFF_MS)
}

/// One upstream attempt. `Ok` on any HTTP status (including 4xx/5xx);
/// `Err` only on transport failure.
fn attempt(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<UpstreamResponse, String> {
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_bytes(body) {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp
                .headers_names()
                .iter()
                .filter_map(|n| resp.header(n).map(|v| (n.clone(), v.to_string())))
                .filter(|(n, _)| !hop_by_hop(n))
                .collect();
            Ok(UpstreamResponse {
                status,
                headers,
                body: resp.into_reader(),
            })
        }
        Err(ureq::Error::Status(code, resp)) => {
            // The status is the answer; the body rides along as the error.
            let mut text = resp.into_string().unwrap_or_default();
            text.truncate(MAX_ERROR_BODY);
            Err(format!("HTTP {code}: {text}"))
        }
        Err(e) => Err(format!("transport: {e}")),
    }
}

/// Parse the `HTTP <code>: ...` message `attempt` produced for a
/// status error back into its code.
fn status_of(msg: &str) -> Option<u16> {
    msg.strip_prefix("HTTP ")?
        .split(':')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Forward one request with retries. Returns the response (any HTTP
/// status) and how many retries were performed, or the last failure.
pub fn forward_with_retry(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    max_retries: u32,
    backoff_base_ms: u64,
) -> Result<(UpstreamResponse, u32), ForwardError> {
    let mut retries = 0;
    loop {
        match attempt(agent, url, headers, body) {
            Ok(resp) => {
                if is_retryable_status(resp.status) && retries < max_retries {
                    // Drain the body before retrying so the connection
                    // is not left half-read.
                    let mut drained = resp.body;
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = drained.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(backoff_ms(backoff_base_ms, retries)));
                    retries += 1;
                    continue;
                }
                return Ok((resp, retries));
            }
            Err(msg) => {
                if let Some(code) = status_of(&msg) {
                    if is_retryable_status(code) && retries < max_retries {
                        std::thread::sleep(Duration::from_millis(backoff_ms(
                            backoff_base_ms,
                            retries,
                        )));
                        retries += 1;
                        continue;
                    }
                    // Final answer: the upstream error body, verbatim.
                    let body = msg
                        .strip_prefix(&format!("HTTP {code}: "))
                        .unwrap_or("")
                        .as_bytes()
                        .to_vec();
                    return Err(ForwardError::Upstream { status: code, body });
                }
                if retries < max_retries {
                    std::thread::sleep(Duration::from_millis(backoff_ms(backoff_base_ms, retries)));
                    retries += 1;
                    continue;
                }
                return Err(ForwardError::Transport(msg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses_and_backoff() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
        assert_eq!(backoff_ms(500, 0), 500);
        assert_eq!(backoff_ms(500, 1), 1000);
        assert_eq!(backoff_ms(500, 2), 2000);
        assert_eq!(backoff_ms(500, 10), 8000);
        assert_eq!(status_of("HTTP 429: slow down"), Some(429));
        assert_eq!(status_of("transport: reset"), None);
    }
}
