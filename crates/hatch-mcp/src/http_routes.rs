//! Route handlers for the streamable-HTTP MCP server: `POST`, `GET`
//! and `DELETE /mcp`. This is a second `impl` block for
//! [`HttpServer`](crate::http_server::HttpServer); it lives here only
//! to keep each file under the 400-line limit. The handlers call the
//! shared [`Server`](crate::server::Server) through
//! [`HttpServer::handle_line`](crate::http_server::HttpServer::handle_line),
//! so the gatekeeper/approval path is identical to stdio.

use crate::http_auth::Auth;
use crate::http_proto::{rpc_error, write_json, write_response, HttpRequest};
use crate::http_server::HttpServer;
use serde_json::Value;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Seconds between `: ping` comments on a `GET /mcp` SSE stream.
const SSE_HEARTBEAT_SECS: u64 = 15;

impl HttpServer {
    /// POST /mcp: one JSON-RPC request or notification. `auth` is this
    /// request's authenticated identity: its origin is stamped on any
    /// approval the call queues, and its scope gates write tools.
    pub(crate) fn handle_post(
        &self,
        stream: &mut dyn Write,
        req: &HttpRequest,
        auth: &Auth,
    ) -> (u16, String) {
        let body = String::from_utf8_lossy(&req.body).into_owned();
        let parsed: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                let reply = rpc_error(&Value::Null, -32700, "Parse error");
                let _ = write_json(stream, 400, "Bad Request", &[], &reply);
                return (400, String::new());
            }
        };
        if parsed.is_array() {
            let reply = rpc_error(&Value::Null, -32600, "batch requests are not supported");
            let _ = write_json(stream, 400, "Bad Request", &[], &reply);
            return (400, String::new());
        }
        let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
        let short = req.session_short();
        if method == "initialize" {
            let Some(sid) = self.create_session() else {
                let _ = write_response(
                    stream,
                    429,
                    "Too Many Requests",
                    &[("Retry-After", "1")],
                    b"too many sessions",
                );
                return (429, String::new());
            };
            return match self.handle_line(&body, auth) {
                Some(reply) => {
                    let headers = [("Mcp-Session-Id", sid.as_str())];
                    let status = write_rpc_reply(stream, req, &reply, &headers);
                    (status, sid.chars().take(8).collect())
                }
                None => {
                    let _ = write_response(stream, 202, "Accepted", &[], b"");
                    (202, sid.chars().take(8).collect())
                }
            };
        }
        let Some(sid) = req.session_id() else {
            let id = parsed.get("id").cloned().unwrap_or(Value::Null);
            let reply = rpc_error(&id, -32600, "missing Mcp-Session-Id header");
            let _ = write_json(stream, 400, "Bad Request", &[], &reply);
            return (400, String::new());
        };
        if !self.touch_session(sid) {
            let id = parsed.get("id").cloned().unwrap_or(Value::Null);
            let reply = rpc_error(&id, -32600, "unknown session");
            let _ = write_json(stream, 404, "Not Found", &[], &reply);
            return (404, short);
        }
        match self.handle_line(&body, auth) {
            Some(reply) => {
                let headers = [("Mcp-Session-Id", sid)];
                let status = write_rpc_reply(stream, req, &reply, &headers);
                (status, short)
            }
            // Notification: no reply, 202.
            None => {
                let _ = write_response(stream, 202, "Accepted", &[], b"");
                (202, short)
            }
        }
    }

    /// GET /mcp: hold an SSE stream open for one session. No server
    /// notifications exist yet, so only `: connected` and heartbeats go
    /// out; the stream closes when the client disconnects, the session
    /// is deleted, or the server shuts down.
    pub(crate) fn handle_get(
        &self,
        stream: &mut dyn Write,
        req: &HttpRequest,
        shutdown: &AtomicBool,
    ) -> (u16, String) {
        let Some(sid) = req.session_id() else {
            let _ = write_response(stream, 400, "Bad Request", &[], b"missing Mcp-Session-Id");
            return (400, String::new());
        };
        let short = req.session_short();
        if !self.touch_session(sid) {
            let _ = write_response(stream, 404, "Not Found", &[], b"unknown session");
            return (404, short);
        }
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n: connected\n\n";
        if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
            return (200, short);
        }
        let sid = sid.to_string();
        let mut ticks = 0u64;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            ticks += 1;
            if shutdown.load(Ordering::SeqCst) {
                break; // Server shutting down.
            }
            if !self.touch_session(&sid) {
                break; // Session deleted via DELETE /mcp.
            }
            if ticks.is_multiple_of(SSE_HEARTBEAT_SECS)
                && (stream.write_all(b": ping\n\n").is_err() || stream.flush().is_err())
            {
                break; // Client gone.
            }
        }
        (200, short)
    }

    /// DELETE /mcp: terminate a session.
    pub(crate) fn handle_delete(&self, stream: &mut dyn Write, req: &HttpRequest) -> (u16, String) {
        let Some(sid) = req.session_id() else {
            let _ = write_response(stream, 400, "Bad Request", &[], b"missing Mcp-Session-Id");
            return (400, String::new());
        };
        let short = req.session_short();
        if !self.remove_session(sid) {
            let _ = write_response(stream, 404, "Not Found", &[], b"unknown session");
            return (404, short);
        }
        let _ = write_response(stream, 204, "No Content", &[], b"");
        (204, short)
    }
}

/// Write one JSON-RPC reply: plain JSON, or an SSE `data:` event when
/// the client asked for `text/event-stream`.
fn write_rpc_reply(
    stream: &mut dyn Write,
    req: &HttpRequest,
    reply: &str,
    extra: &[(&str, &str)],
) -> u16 {
    let wants_sse = req
        .header("accept")
        .map(|a| a.contains("text/event-stream"))
        .unwrap_or(false);
    if wants_sse {
        let body = format!("data: {reply}\n\n");
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "text/event-stream")];
        headers.extend_from_slice(extra);
        let _ = write_response(stream, 200, "OK", &headers, body.as_bytes());
    } else {
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        headers.extend_from_slice(extra);
        let _ = write_response(stream, 200, "OK", &headers, reply.as_bytes());
    }
    200
}
