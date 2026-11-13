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
    while let Ok(events) = log.events_since(id, cursor, 100) {
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

#[cfg(test)]
mod tests {
    use super::super::server_tests::{authed, env, raw, start};
    use crate::commands::task_view::detail;
    use hatch_core::time::now;
    use hatch_tasks::{EventLog, TaskStatus, TaskStore};
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// Seed two tasks (A brief, B invite) and three control events on
    /// A. Returns the two task ids.
    fn seed(ctx: &super::ApiCtx) -> (String, String) {
        let db = &ctx.app.db;
        let t = now();
        let a = TaskStore::new(db)
            .create("meeting-brief", "API 测试简报", "daemon", "", 3, None, &t)
            .unwrap();
        let b = TaskStore::new(db)
            .create("meeting-invite", "API 测试邀请", "daemon", "", 3, None, &t)
            .unwrap();
        let log = EventLog::new(db);
        for (kind, step) in [("step_done", "triage"), ("step_done", "gather"), ("task_paused", "")] {
            log.append_control_event(&a.id, kind, step, "seed", &t).unwrap();
        }
        (a.id, b.id)
    }

    /// Read an SSE stream until `want` task_event frames arrive or
    /// the deadline passes; returns the (seq, data) pairs in arrival
    /// order. The stream stays open by design, so this never waits
    /// for EOF.
    fn read_sse(
        addr: SocketAddr,
        path: &str,
        headers: &[(&str, &str)],
        want: usize,
        wait_ms: u64,
    ) -> Vec<(i64, Value)> {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if Instant::now() >= deadline || parse_task_events(&buf).len() >= want {
                break;
            }
            match stream.read(&mut chunk) {
                Ok(0) => break, // server closed
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => {} // read timeout: keep waiting until the deadline
            }
        }
        parse_task_events(&buf)
    }

    /// Parse `event: task_event` frames out of a raw SSE byte buffer.
    fn parse_task_events(buf: &[u8]) -> Vec<(i64, Value)> {
        let text = String::from_utf8_lossy(buf);
        let mut out = Vec::new();
        for frame in text.split("\n\n") {
            if !frame.contains("event: task_event") {
                continue;
            }
            let seq = frame
                .lines()
                .find_map(|l| l.strip_prefix("id: "))
                .and_then(|s| s.trim().parse::<i64>().ok());
            let data = frame
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .and_then(|s| serde_json::from_str::<Value>(s).ok());
            if let (Some(seq), Some(data)) = (seq, data) {
                out.push((seq, data));
            }
        }
        out
    }

    fn sse_headers(token: &str) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", format!("Bearer {token}")),
            ("Accept", "text/event-stream".to_string()),
        ]
    }

    /// The list and the detail are the same projection the CLI
    /// reads: identical JSON from `task show` and `GET /v1/tasks/{id}`.
    #[test]
    fn api_list_and_get_match_cli_projection() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let (a, b) = seed(&env.ctx);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (status, _, body) = raw(addr, "GET", "/v1/tasks", &auth_refs, "");
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        let tasks = v["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2, "两个任务应各出现一次");
        let ids: Vec<&str> = tasks.iter().map(|t| t["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
        let row_a = tasks.iter().find(|t| t["id"] == a).unwrap();
        assert_eq!(row_a["steps_done"], 0);
        assert_eq!(row_a["steps_total"], 0);
        assert_eq!(row_a["pending_approvals"], 0);

        // The detail is byte-identical to the CLI's `detail()`.
        let (status, _, body) = raw(addr, "GET", &format!("/v1/tasks/{a}"), &auth_refs, "");
        assert_eq!(status, 200);
        let via_api: Value = serde_json::from_str(&body).unwrap();
        let via_cli = serde_json::to_value(detail(&env.ctx.app.db, &a).unwrap()).unwrap();
        assert_eq!(via_api, via_cli, "API 与 CLI 必须是同一投影");

        // Filters, missing tasks, and auth behave.
        let (status, _, body) = raw(addr, "GET", "/v1/tasks?status=done", &auth_refs, "");
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["tasks"].as_array().unwrap().len(), 0);
        let (status, _, _) = raw(addr, "GET", "/v1/tasks/tsk_missing", &auth_refs, "");
        assert_eq!(status, 404);
        let (status, _, _) = raw(addr, "GET", "/v1/tasks", &[], "");
        assert_eq!(status, 401, "列表需要 token");
        shutdown.store(true, Ordering::SeqCst);
    }

    /// `POST /v1/tasks/{id}/cancel` cancels exactly like
    /// `hatch task cancel`, and the store agrees afterwards.
    #[test]
    fn api_cancel_matches_cli_cancel() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let (a, _) = seed(&env.ctx);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (status, _, body) = raw(
            addr,
            "POST",
            &format!("/v1/tasks/{a}/cancel"),
            &auth_refs,
            "{\"note\": \"api cancel\"}",
        );
        assert_eq!(status, 200);
        assert!(body.contains("已取消"), "取消应确认，实际 {body}");
        let task = TaskStore::new(&env.ctx.app.db).get(&a).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Cancelled);
        let (status, _, body) = raw(addr, "GET", &format!("/v1/tasks/{a}"), &auth_refs, "");
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["status"], "cancelled");

        // A second cancel on a terminal task is refused, not a silent no-op.
        let (status, _, _) = raw(
            addr,
            "POST",
            &format!("/v1/tasks/{a}/cancel"),
            &auth_refs,
            "{}",
        );
        assert_ne!(status, 200, "终态任务再取消应被拒");
        shutdown.store(true, Ordering::SeqCst);
    }

    /// SSE replay sends each event exactly once, in order, and a
    /// reconnect at the last seen id sends nothing new.
    #[test]
    fn sse_replay_sends_each_event_once_and_resume_sends_nothing_new() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let (a, _) = seed(&env.ctx);

        // Full replay from the start.
        let h = sse_headers("tok-123");
        let h_refs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let frames = read_sse(addr, &format!("/v1/tasks/{a}/events"), &h_refs, 3, 2000);
        assert_eq!(frames.len(), 3, "应重放 3 个事件，各一次");
        assert_eq!(frames.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(frames[0].1["kind"], "step_done");
        assert_eq!(frames[2].1["kind"], "task_paused");

        // Reconnect at the last seen id: nothing new arrives.
        let mut h2 = sse_headers("tok-123");
        h2.push(("Last-Event-ID", "3".to_string()));
        let h2_refs: Vec<(&str, &str)> = h2.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let frames = read_sse(addr, &format!("/v1/tasks/{a}/events"), &h2_refs, 1, 900);
        assert_eq!(frames.len(), 0, "游标之后的事件应为空，不得重发");

        // `?after=` is the same cursor without the header.
        let frames = read_sse(addr, &format!("/v1/tasks/{a}/events?after=1"), &h_refs, 2, 2000);
        assert_eq!(frames.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![2, 3]);
        shutdown.store(true, Ordering::SeqCst);
    }

    /// The JSON events endpoint replays identically on repeat reads.
    #[test]
    fn events_json_replay_is_idempotent() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let (a, _) = seed(&env.ctx);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (status, _, first) =
            raw(addr, "GET", &format!("/v1/tasks/{a}/events?after=0"), &auth_refs, "");
        assert_eq!(status, 200);
        let (status, _, second) =
            raw(addr, "GET", &format!("/v1/tasks/{a}/events?after=0"), &auth_refs, "");
        assert_eq!(status, 200);
        assert_eq!(first, second, "两次重放应得到相同的 JSON");
        let v: Value = serde_json::from_str(&first).unwrap();
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|e| e["seq"].as_i64().unwrap()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let (status, _, body) =
            raw(addr, "GET", &format!("/v1/tasks/{a}/events?after=2"), &auth_refs, "");
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["events"].as_array().unwrap().len(), 1);
        shutdown.store(true, Ordering::SeqCst);
    }
}
