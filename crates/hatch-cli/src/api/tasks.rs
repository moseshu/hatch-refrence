//! Task endpoints (task 5): the planning list and control plane for
//! external clients, over the same ledger the CLI reads — one
//! projection, two clients, no second state.
//!
//! - `GET /v1/tasks` — list summaries (`?status=`, `?limit=`).
//! - `GET /v1/tasks/{id}` — the full detail (plan card).
//! - `GET /v1/tasks/{id}/events` — SSE when the client asks for
//!   `text/event-stream` (`Last-Event-ID` / `?after=` cursor replay,
//!   then live events until disconnect); plain JSON otherwise.
//! - `POST /v1/tasks/{id}/cancel` — cancel (body `{"note"?}`).

use super::http::SseWriter;
use super::{json_body, opt_str, ApiCtx, ApiError};
use crate::commands::task_control::cancel_task;
use crate::commands::task_view::{detail, list_summaries};
use hatch_core::time::now;
use hatch_mcp::http_proto::{write_json, HttpRequest};
use hatch_store::Db;
use hatch_tasks::{EventLog, TaskStatus, TaskStore};
use serde_json::{json, Value};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// `GET /v1/tasks` — `?status=` filters (queued/running/waiting/
/// blocked/done/failed/cancelled), `?limit=` caps the rows
/// (default 50).
pub fn list(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let status = match req.query_param("status") {
        Some(s) => Some(parse_status(&s)?),
        None => None,
    };
    let limit = req
        .query_param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50);
    let tasks = list_summaries(&ctx.app.db, status, limit)?;
    Ok((200, json!({ "tasks": tasks })))
}

/// `GET /v1/tasks/{id}` — the full detail, same projection as
/// `task show`.
pub fn get(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let d = detail(&ctx.app.db, id)?;
    Ok((200, serde_json::to_value(d).unwrap_or(Value::Null)))
}

/// `GET /v1/tasks/{id}/events` without the SSE accept header — the
/// same events as JSON (`?after=`, `?limit=`).
pub fn events(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    require_task(&ctx.app.db, id)?;
    let after = req
        .query_param("after")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let limit = req
        .query_param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50);
    let log = EventLog::new(&ctx.app.db);
    let events = log.events_since(id, after, limit)?;
    Ok((200, json!({ "events": events })))
}

/// `POST /v1/tasks/{id}/cancel` — body `{"note"?}`. Cancels the
/// task and invalidates its pending approvals, exactly like
/// `hatch task cancel`.
pub fn cancel(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let note = opt_str(&body, "note").unwrap_or_default();
    let msg = cancel_task(&ctx.app.db, id, &note, &now())?;
    Ok((200, json!({ "message": msg })))
}

fn parse_status(s: &str) -> Result<TaskStatus, ApiError> {
    match s {
        "queued" => Ok(TaskStatus::Queued),
        "running" => Ok(TaskStatus::Running),
        "waiting" => Ok(TaskStatus::Waiting),
        "blocked" => Ok(TaskStatus::Blocked),
        "done" => Ok(TaskStatus::Done),
        "failed" => Ok(TaskStatus::Failed),
        "cancelled" => Ok(TaskStatus::Cancelled),
        _ => Err(ApiError::bad_request(format!(
            "未知任务状态 {s}（queued/running/waiting/blocked/done/failed/cancelled）"
        ))),
    }
}

fn require_task(db: &Db, id: &str) -> Result<(), ApiError> {
    if TaskStore::new(db).get(id)?.is_none() {
        return Err(ApiError::not_found("任务"));
    }
    Ok(())
}

/// `GET /v1/tasks/{id}/events` — SSE. Replays every event after the
/// cursor, then polls for new events until the client disconnects.
/// Each frame carries its sequence as the SSE `id`, so a
/// reconnecting client resumes exactly where it left off: no loss,
/// no duplication.
pub fn handle_events_stream(ctx: &ApiCtx, stream: &mut TcpStream, id: &str, after: i64) -> u16 {
    // The task must exist before the stream starts; a missing task
    // is a plain 404, not an SSE error frame.
    if let Err(e) = require_task(&ctx.app.db, id) {
        let reason = if e.status == 404 {
            "Not Found"
        } else {
            "Internal Server Error"
        };
        let _ = write_json(stream, e.status, reason, &[], &e.to_json().to_string());
        return e.status;
    }
    let writer = match SseWriter::start(stream) {
        Ok(w) => w,
        Err(_) => return 200,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let _heartbeat = writer.heartbeat(Arc::clone(&stop));
    let _ = writer.comment("connected");
    let log = EventLog::new(&ctx.app.db);
    let mut cursor = after;
    loop {
        let events = match log.events_since(id, cursor, 100) {
            Ok(e) => e,
            Err(_) => break,
        };
        for ev in events {
            let data = json!({
                "seq": ev.seq,
                "kind": ev.kind,
                "step_id": ev.step_id,
                "detail": ev.detail,
                "created_at": ev.created_at,
            })
            .to_string();
            if writer.event_with_id(ev.seq, "task_event", &data).is_err() {
                stop.store(true, Ordering::SeqCst);
                return 200;
            }
            cursor = ev.seq;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    stop.store(true, Ordering::SeqCst);
    200
}
