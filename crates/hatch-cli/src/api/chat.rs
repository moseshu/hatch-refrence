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
//! token is removed when the turn ends either way. One turn at a time
//! per session — a second message while one runs gets 409.

use super::http::SseWriter;
use super::{json_body, req_str, ApiCtx, ApiError};
use crate::commands::chat::{background, setup_turn, TurnSetup};
use crate::commands::chat_drain::drain_compaction;
use hatch_agent::{Agent, AgentConfig, ProgressKind, RunOptions, SessionStore, StopReason};
use hatch_agent::TurnReport;
use hatch_core::CancelToken;
use hatch_llm::{Router, StreamEvent};
use hatch_mcp::http_proto::{write_json, HttpRequest};
use serde_json::{json, Value};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The turn report as API JSON.
pub fn report_json(r: &TurnReport) -> Value {
    let (stop, detail) = match &r.stop {
        StopReason::Answered => ("answered", None),
        StopReason::StepLimit => ("step_limit", None),
        StopReason::Budget(m) => ("budget", Some(m.clone())),
        StopReason::ModelError(m) => ("model_error", Some(m.clone())),
        StopReason::Cancelled(m) => ("cancelled", Some(m.clone())),
    };
    json!({
        "answer": r.answer,
        "stop": stop,
        "stop_detail": detail,
        "steps": r.steps,
        "tool_calls": r.tool_calls,
        "approvals": r.approvals,
        "compacted": r.compacted,
        "compaction_requested": r.compaction_requested,
        "model_calls": r.model_calls,
        "usage": {
            "prompt_tokens": r.usage.prompt_tokens,
            "completion_tokens": r.usage.completion_tokens,
            "cached_tokens": r.usage.cached_tokens,
            "reasoning_tokens": r.usage.reasoning_tokens,
        },
        "tainted": r.tainted,
        "subagents": r.subagents,
    })
}

/// Shared pre-flight for both turn shapes: the session must exist, no
/// turn may be running for it, and a router must be buildable. Returns
/// the router, the turn setup, background handoff lines and a fresh
/// cancel token (already registered in `ctx.running`).
fn prepare_turn(
    ctx: &ApiCtx,
    session: &str,
) -> Result<(Router, TurnSetup, Vec<String>, CancelToken), ApiError> {
    if !SessionStore::new(&ctx.app.db).exists(session)? {
        return Err(ApiError::not_found("会话"));
    }
    if ctx.running.lock().unwrap().contains_key(session) {
        return Err(ApiError::conflict("该会话已有进行中的回合"));
    }
    let router = (ctx.router_factory)()?;
    let setup = setup_turn(&ctx.app, None)?;
    let bg = background(&ctx.app)?;
    drain_compaction(&ctx.app, session);
    let cancel = CancelToken::new();
    {
        let mut running = ctx.running.lock().unwrap();
        if running.contains_key(session) {
            return Err(ApiError::conflict("该会话已有进行中的回合"));
        }
        running.insert(session.to_string(), cancel.clone());
    }
    Ok((router, setup, bg, cancel))
}

fn finish_turn(ctx: &ApiCtx, session: &str) {
    ctx.running.lock().unwrap().remove(session);
}

/// Build the [`Agent`] for an API turn. Approvals from API turns carry
/// origin `api:<session>` so they are attributable in the queue.
fn build_agent<'a>(
    ctx: &'a ApiCtx,
    router: &'a Router,
    setup: &'a TurnSetup,
    bg: Vec<String>,
    session: &str,
) -> Agent<'a> {
    Agent {
        db: &ctx.app.db,
        router,
        tools: &setup.tools,
        executors: &setup.executors,
        gate: &setup.gate,
        config: AgentConfig {
            user_name: ctx.app.config.user.name.clone(),
            user_email: ctx.app.user_email(),
            tz: ctx.app.tz,
            max_tokens: ctx.app.config.limits.max_tokens,
            max_steps: ctx.app.config.agent.max_steps,
            delegation: setup.delegation.clone(),
            background: bg,
            skills: setup.skill_bodies.clone(),
            skill_tools: setup.skill_tools.clone(),
            origin: Some(format!("api:{session}")),
            ..Default::default()
        },
    }
}

/// Non-streaming turn: run to completion, return the report as JSON.
pub fn run_turn_json(
    ctx: &ApiCtx,
    session: &str,
    body: &Value,
) -> Result<(u16, Value), ApiError> {
    let text = req_str(body, "text")?;
    let (router, setup, bg, cancel) = prepare_turn(ctx, session)?;
    let agent = build_agent(ctx, &router, &setup, bg, session);
    let now = hatch_core::time::now();
    let result = agent.run(
        session,
        &text,
        &now,
        RunOptions {
            on_delta: None,
            cancel: cancel.clone(),
            observer: None,
        },
    );
    finish_turn(ctx, session);
    if let Err(e) = ctx.app.record_usage(&router, &now) {
        eprintln!("[api] 记录用量失败：{e:#}");
    }
    match result {
        Ok(report) => Ok((200, report_json(&report))),
        Err(e) => Err(ApiError::internal(format!("回合失败：{e}"))),
    }
}

/// Cancel a running turn for `session`. 200 with `cancelled: false`
/// when no turn is running; the session itself must exist.
pub fn cancel_turn(ctx: &ApiCtx, session: &str) -> Result<(u16, Value), ApiError> {
    if !SessionStore::new(&ctx.app.db).exists(session)? {
        return Err(ApiError::not_found("会话"));
    }
    let cancelled = ctx
        .running
        .lock()
        .unwrap()
        .get(session)
        .map(|t| {
            t.cancel("api cancel");
            true
        })
        .unwrap_or(false);
    Ok((200, json!({"session": session, "cancelled": cancelled})))
}

/// SSE chat turn. Owns `stream` for the whole turn; returns the HTTP
/// status for the server log line. Pre-flight failures (bad body,
/// unknown session, turn already running, no model) are plain JSON
/// errors because the SSE stream has not started yet; once it has,
/// turn-level failures arrive as an SSE `error` event.
pub fn handle_message_stream(
    ctx: &ApiCtx,
    stream: &mut TcpStream,
    req: &HttpRequest,
    session: &str,
) -> u16 {
    let body = match json_body(req) {
        Ok(b) => b,
        Err(e) => {
            let _ = write_json(stream, e.status, "Bad Request", &[], &e.to_json().to_string());
            return e.status;
        }
    };
    let text = match req_str(&body, "text") {
        Ok(t) => t,
        Err(e) => {
            let _ = write_json(stream, e.status, "Bad Request", &[], &e.to_json().to_string());
            return e.status;
        }
    };
    let (router, setup, bg, cancel) = match prepare_turn(ctx, session) {
        Ok(t) => t,
        Err(e) => {
            let reason = match e.status {
                404 => "Not Found",
                409 => "Conflict",
                503 => "Service Unavailable",
                _ => "Internal Server Error",
            };
            let _ = write_json(stream, e.status, reason, &[], &e.to_json().to_string());
            return e.status;
        }
    };
    let writer = match SseWriter::start(stream) {
        Ok(w) => w,
        Err(_) => {
            finish_turn(ctx, session);
            return 200; // client is gone
        }
    };
    let _ = writer.comment("connected");
    let stop = Arc::new(AtomicBool::new(false));
    let heartbeat = writer.heartbeat(stop.clone());
    let w1 = writer.clone();
    let mut sink = move |e: StreamEvent| match e {
        StreamEvent::TextDelta(t) => {
            let _ = w1.event("message", &json!({"delta": t}).to_string());
        }
        // Tool calls arrive via the observer (with arguments); the
        // sink's start notification would only duplicate them.
        StreamEvent::ToolCallStarted { .. } | StreamEvent::Done => {}
    };
    let w2 = writer.clone();
    let observer = move |p: &hatch_agent::Progress| match p.kind {
        ProgressKind::ToolCall => {
            let _ = w2.event("tool_call", &json!({"detail": p.detail}).to_string());
        }
        ProgressKind::ToolResult => {
            let _ = w2.event("tool_result", &json!({"detail": p.detail}).to_string());
        }
        ProgressKind::Delegate => {
            let _ = w2.event(
                "tool_call",
                &json!({"detail": p.detail, "kind": "delegate"}).to_string(),
            );
        }
        ProgressKind::SubagentDone => {
            let _ = w2.event(
                "tool_result",
                &json!({"detail": p.detail, "kind": "subagent_done"}).to_string(),
            );
        }
        _ => {}
    };
    let agent = build_agent(ctx, &router, &setup, bg, session);
    let now = hatch_core::time::now();
    let result = agent.run(
        session,
        &text,
        &now,
        RunOptions {
            on_delta: Some(&mut sink),
            cancel: cancel.clone(),
            observer: Some(&observer),
        },
    );
    finish_turn(ctx, session);
    stop.store(true, Ordering::SeqCst);
    let _ = heartbeat.join();
    if let Err(e) = ctx.app.record_usage(&router, &now) {
        eprintln!("[api] 记录用量失败：{e:#}");
    }
    match result {
        Ok(report) => {
            if !report.approvals.is_empty() {
                let _ = writer.event(
                    "approval_needed",
                    &json!({"approval_ids": report.approvals}).to_string(),
                );
            }
            let _ = writer.event("done", &report_json(&report).to_string());
            200
        }
        Err(e) => {
            let _ = writer.event(
                "error",
                &json!({"error": {"code": "turn_failed", "message": e.to_string()}}).to_string(),
            );
            200
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use hatch_agent::SessionStore;
    use hatch_llm::scripted::ScriptedModel;
    use hatch_llm::{ChatResponse, Router};
    use hatch_mcp::http_proto::parse_request;
    use std::io::{BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    fn scripted_ctx(answer: &str) -> (tempfile::TempDir, Arc<ApiCtx>, String) {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        let session =
            SessionStore::new(&app.db).create("test", &hatch_core::time::now()).unwrap();
        let model = Arc::new(ScriptedModel::ok(
            "test",
            vec![ChatResponse::text(answer)],
        ));
        let ctx = Arc::new(
            ApiCtx::new(app).with_router_factory(Arc::new(move || {
                Ok(Router::single(model.clone()))
            })),
        );
        (dir, ctx, session)
    }

    #[test]
    fn json_turn_answers_with_scripted_model() {
        let (_dir, ctx, session) = scripted_ctx("你好，我是测试助手");
        let (status, body) =
            run_turn_json(&ctx, &session, &json!({"text": "你好"})).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["answer"], "你好，我是测试助手");
        assert_eq!(body["stop"], "answered");
        assert_eq!(body["model_calls"], 1);
        // The answer is persisted in the session history.
        let (_, history) = SessionStore::new(&ctx.app.db).active(&session).unwrap();
        assert!(history
            .iter()
            .any(|m| m.message.content.contains("测试助手")));
        assert!(!ctx.running.lock().unwrap().contains_key(&session));
    }

    #[test]
    fn missing_session_is_404_and_missing_text_is_400() {
        let (_dir, ctx, session) = scripted_ctx("x");
        let err = run_turn_json(&ctx, "no-such-session", &json!({"text": "hi"})).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "not_found");
        let err = run_turn_json(&ctx, &session, &json!({})).unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn no_model_is_503() {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        let session =
            SessionStore::new(&app.db).create("test", &hatch_core::time::now()).unwrap();
        let ctx = Arc::new(ApiCtx::new(app));
        let err = run_turn_json(&ctx, &session, &json!({"text": "hi"})).unwrap_err();
        assert_eq!(err.status, 503);
        assert_eq!(err.code, "model_not_configured");
    }

    #[test]
    fn cancel_without_running_turn_reports_false() {
        let (_dir, ctx, session) = scripted_ctx("x");
        let (status, body) = cancel_turn(&ctx, &session).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["cancelled"], false);
        let err = cancel_turn(&ctx, "no-such-session").unwrap_err();
        assert_eq!(err.status, 404);
    }

    #[test]
    fn sse_turn_streams_message_and_done() {
        let (_dir, ctx, session) = scripted_ctx("你好，我是测试助手");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let ctx2 = ctx.clone();
        let session2 = session.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let req = parse_request(&mut reader).unwrap();
            handle_message_stream(&ctx2, &mut stream, &req, &session2)
        });
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let body = r#"{"text":"你好"}"#;
        let req = format!(
            "POST /v1/sessions/{session}/messages HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        client.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.contains("Content-Type: text/event-stream"), "{text}");
        assert!(text.contains(": connected"), "{text}");
        assert!(text.contains("event: message"), "{text}");
        assert!(text.contains("测试助手"), "{text}");
        assert!(text.contains("event: done"), "{text}");
        assert!(text.contains(r#""stop":"answered""#), "{text}");
        assert_eq!(server.join().unwrap(), 200);
        assert!(!ctx.running.lock().unwrap().contains_key(&session));
    }
}
