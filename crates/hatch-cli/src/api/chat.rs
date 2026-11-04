//! Chat turns over the product API: `POST /v1/sessions/{id}/messages`.
//!
//! Two shapes, decided by `Accept`:
//!
//! * `Accept: text/event-stream` — the turn streams SSE events
//!   (`message` deltas, `tool_call` / `tool_result` progress,
//!   `approval_needed`, then `done` with the turn report). The turn
//!   runs on the connection thread with the model's own streaming sink,
//!   so deltas leave while the model is still generating.
//! * Anything else — the turn runs to completion and the report comes
//!   back as one JSON body.
//!
//! A turn registers its [`CancelToken`] in `ctx.running` under the
//! session id, so `POST /v1/sessions/{id}/cancel` can stop it; the
//! token is removed when the turn ends either way.

use super::ApiCtx;
use hatch_mcp::http_proto::HttpRequest;
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;

/// SSE chat turn. Owns `stream` for the whole turn; returns the HTTP
/// status for the server log line (always 200 — turn-level failures
/// arrive as an SSE `error` event, not as an HTTP error).
pub fn handle_message_stream(
    _ctx: &ApiCtx,
    _stream: &mut TcpStream,
    _req: &HttpRequest,
    _session: &str,
    _shutdown: &AtomicBool,
) -> u16 {
    // TODO (step 2): run the turn with the SSE sink.
    501
}
