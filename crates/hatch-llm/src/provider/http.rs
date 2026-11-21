//! Shared blocking HTTP calls with error classification.
//!
//! Every request goes through [`agent_for`], which applies the proxy and CA
//! settings from [`super::net`] — buffered and streaming calls alike.

use super::sse::{SseEvent, SseParser};
use crate::LlmError;
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

fn agent_for(url: &str, timeout: Option<Duration>) -> Result<ureq::Agent, LlmError> {
    // Streams can legitimately run long: bound connect and idle reads
    // instead of one overall timeout. Agents are cached per
    // (proxy, timeout shape) so the connection pool survives across
    // requests — see `net::cached_agent`.
    let timeout = match timeout {
        Some(t) => super::net::AgentTimeout::Overall(t),
        None => super::net::AgentTimeout::Stream,
    };
    super::net::cached_agent(url, timeout, None).map_err(LlmError::permanent)
}

fn send(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    timeout: Option<Duration>,
    accept: &str,
) -> Result<ureq::Response, (u16, String, LlmError)> {
    let agent = agent_for(url, timeout).map_err(|e| (0, String::new(), e))?;
    let mut req = agent.post(url).set("Content-Type", "application/json").set("Accept", accept);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_json(body.clone()) {
        Ok(resp) => Ok(resp),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let snippet: String = text.chars().take(400).collect();
            let msg = format!("HTTP {code}: {snippet}");
            let err = if code == 429 || code >= 500 { LlmError::transient(msg) } else { LlmError::permanent(msg) };
            Err((code, snippet, err))
        }
        Err(e) => Err((0, String::new(), LlmError::transient(format!("transport: {e}")))),
    }
}

pub(crate) fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    timeout: Duration,
) -> Result<Value, LlmError> {
    let resp = send(url, headers, body, Some(timeout), "application/json").map_err(|(_, _, e)| e)?;
    // Read and parse separately. A body that cannot be read to the end
    // (connection dropped mid-response, truncated chunked framing) is a
    // network failure, so it is transient and the router retries it; a
    // body that arrives whole but is not JSON will not parse on a retry.
    let text = resp
        .into_string()
        .map_err(|e| LlmError::transient(format!("read: {e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| LlmError::permanent(format!("response is not JSON: {e}")))
}

/// Status codes that mean "this endpoint/parameter combination does not do
/// streaming" rather than "the request is wrong": worth one buffered retry.
fn stream_rejected(code: u16) -> bool {
    matches!(code, 400 | 404 | 405 | 406 | 415 | 422 | 501)
}

/// POST and consume a Server-Sent Events response, calling `on_event` for
/// each event until it returns `Ok(false)` or the stream ends.
pub(crate) fn post_sse(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    on_event: &mut dyn FnMut(SseEvent) -> Result<bool, LlmError>,
) -> Result<(), LlmError> {
    let resp = match send(url, headers, body, None, "text/event-stream") {
        Ok(r) => r,
        Err((code, snippet, _)) if stream_rejected(code) => {
            return Err(LlmError::stream_rejected(format!("HTTP {code}: {snippet}")));
        }
        Err((_, _, e)) => return Err(e),
    };
    let ctype = resp.content_type().to_string();
    if ctype.contains("application/json") {
        // Some gateways ignore `stream` and answer with a normal JSON body.
        let text = resp.into_string().map_err(|e| LlmError::transient(format!("read: {e}")))?;
        return on_event(SseEvent { event: Some("__json__".into()), data: text }).map(|_| ());
    }
    let mut reader = resp.into_reader();
    let mut parser = SseParser::new();
    let mut buf = [0u8; 8192];
    loop {
        if crate::cancel::aborted() {
            return Err(LlmError::cancelled("stream abandoned by the caller"));
        }
        let n = reader.read(&mut buf).map_err(|e| LlmError::transient(format!("stream read: {e}")))?;
        if n == 0 {
            break;
        }
        for ev in parser.push(&buf[..n]) {
            if !on_event(ev)? {
                return Ok(());
            }
        }
    }
    if let Some(ev) = parser.finish() {
        on_event(ev)?;
    }
    Ok(())
}
