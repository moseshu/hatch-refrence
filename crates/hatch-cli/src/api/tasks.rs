//! Task endpoints (task 5, P0 product closed loop): the planning list
//! and control plane for external clients, over the same ledger the
//! CLI reads — one projection, two clients, no second state.
//!
//! - `POST /v1/tasks` — create (body: kind, title?, source?, world?,
//!   prompt?, timezone?, gmail_*?, invite?, max_model_calls?,
//!   lease_ttl_secs?, idempotency_key?). Returns 202 + the task id;
//!   a repeated key with identical content returns the same task
//!   (`duplicate: true`), with different content it conflicts (409).
//!   The execution parameters are persisted (schema v20), so the
//!   resident worker resumes the task after a restart with no flags.
//! - `GET /v1/tasks` — list summaries (`?status=`, `?limit=`).
//! - `GET /v1/tasks/{id}` — the full detail (plan card).
//! - `GET /v1/tasks/{id}/events` — SSE when the client asks for
//!   `text/event-stream` (`Last-Event-ID` / `?after=` cursor replay,
//!   then live events until disconnect); plain JSON otherwise.
//! - `POST /v1/tasks/{id}/cancel` — cancel (body `{"note"?}`).
//! - `POST /v1/tasks/{id}/pause` — pause (body `{"note"?}`).
//! - `POST /v1/tasks/{id}/resume` — resume a paused task
//!   (body `{"note"?}`); wakes the resident worker.
//! - `POST /v1/tasks/{id}/command` — mid-task instruction (body
//!   `{"expected_version", "summary"}`); the §10.2 version move.

use super::{json_body, opt_str, req_str, ApiCtx, ApiError};
use crate::commands::task_control::{apply_command, cancel_task, pause_task, resume_task};
use crate::commands::task_run_params::RunParams;
use crate::commands::task_view::{detail, list_summaries};
use hatch_core::time::now;
use hatch_mcp::http_proto::HttpRequest;
use hatch_store::Db;
use hatch_tasks::{CreateOutcome, EventLog, TaskStatus, TaskStore};
use serde_json::{json, Value};

/// `POST /v1/tasks` — create a task on the durable ledger and hand
/// it to the resident worker. The task's mail source and budget are
/// recorded on the row (the approval executor reads them), the full
/// execution parameters go to `task_run_params` in the same
/// transaction (see `TaskStore::create_with_params`). This is a
/// new-task entry: the task-9 flag closes it exactly like
/// `task create`.
pub fn create(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    hatch_release::ensure_new_task_entry(&ctx.app.db).map_err(ApiError::from)?;
    let body = json_body(req)?;
    let params = RunParams::from_create_body(&body).map_err(ApiError::from)?;
    let key = opt_str(&body, "idempotency_key");
    let hash = params.payload_hash().map_err(ApiError::from)?;
    let params_json = params.to_canonical_json().map_err(ApiError::from)?;
    let store = TaskStore::new(&ctx.app.db);
    let outcome = store
        .create_with_params(
            &params.kind,
            &params.title,
            "api",
            "",
            3,
            key.as_deref(),
            &hash,
            &params_json,
            &now(),
        )
        .map_err(ApiError::from)?;
    let (task, duplicate) = match outcome {
        CreateOutcome::Created(t) => (t, false),
        CreateOutcome::Duplicate(t) => (t, true),
        CreateOutcome::Conflict { existing_id } => {
            return Err(ApiError::conflict(format!(
                "idempotency_key 已用于其他内容的任务 {existing_id}（同一键只能提交相同内容）"
            )));
        }
    };
    // The source and budget live on the task row (the approval
    // executor and the budget check read them there, not from the
    // params JSON). Idempotent UPDATEs, so the duplicate path repairs
    // a create that crashed between the transaction and this write.
    // A duplicate of a task that is already running keeps its stored
    // budget — `set_budget` refuses running tasks, and the content
    // is identical anyway (the hash matched).
    store
        .set_source(&task.id, &params.source, &now())
        .map_err(ApiError::from)?;
    if params.max_model_calls > 0
        && matches!(
            task.status,
            TaskStatus::Queued | TaskStatus::Waiting | TaskStatus::Blocked
        )
    {
        store
            .set_budget(&task.id, params.max_model_calls, &now())
            .map_err(ApiError::from)?;
    }
    // The task is queued; the resident worker sweeps within seconds,
    // and this wake skips the wait entirely.
    crate::commands::task_resident::wake(&ctx.app);
    Ok((
        202,
        json!({
            "id": task.id,
            "status": task.status.as_str(),
            "kind": task.kind,
            "title": task.title,
            "duplicate": duplicate,
        }),
    ))
}

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

/// `POST /v1/tasks/{id}/pause` — body `{"note"?}`. Pauses the task
/// at the next step boundary, exactly like `hatch task pause`.
pub fn pause(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let note = opt_str(&body, "note").unwrap_or_default();
    let msg = pause_task(&ctx.app.db, id, &note, &now())?;
    Ok((200, json!({ "message": msg })))
}

/// `POST /v1/tasks/{id}/resume` — body `{"note"?}`. Resumes a
/// paused task to queued and wakes the resident worker, which claims
/// it from the breakpoint — no manual `task run`.
pub fn resume(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let note = opt_str(&body, "note").unwrap_or_default();
    let msg = resume_task(&ctx.app.db, id, &note, &now())?;
    crate::commands::task_resident::wake(&ctx.app);
    Ok((200, json!({ "message": msg })))
}

/// `POST /v1/tasks/{id}/command` — body `{"expected_version", "summary"}`.
/// A mid-task instruction (§10.2): the version the client saw must
/// still be current, or the command is refused as stale. A successful
/// command bumps the plan version and invalidates older approvals;
/// the text itself is recorded, not applied — changing what the
/// steps do is still `edit-step` / P1 work.
pub fn command(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let expected_version = body
        .get("expected_version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ApiError::bad_request("缺少字段 expected_version（数字）"))?;
    let summary = req_str(&body, "summary")?;
    if summary.trim().is_empty() {
        return Err(ApiError::bad_request("summary 不能为空"));
    }
    let msg = apply_command(&ctx.app.db, id, expected_version, &summary, "api", &now())?;
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

pub(crate) fn require_task(db: &Db, id: &str) -> Result<(), ApiError> {
    if TaskStore::new(db).get(id)?.is_none() {
        return Err(ApiError::not_found("任务"));
    }
    Ok(())
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

    /// An idle SSE stream sends a `: ping` keepalive within ~2s: the
    /// keepalive is also the disconnect probe — without it, a client
    /// that goes away while idle leaves the handler thread running
    /// forever and the daemon cannot shut down.
    #[test]
    fn sse_idle_stream_sends_keepalive() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let (a, _) = seed(&env.ctx);
        // Cursor at the last event: nothing new will arrive.
        let mut headers: Vec<(&str, String)> = sse_headers("tok-123");
        headers.push(("Last-Event-ID", "3".to_string()));
        let hrefs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut req = format!("GET /v1/tasks/{a}/events HTTP/1.1\r\nHost: {addr}\r\n");
        for (k, v) in &hrefs {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(4000);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while Instant::now() < deadline {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if String::from_utf8_lossy(&buf).contains(": ping") {
                        break;
                    }
                }
                Err(_) => {} // read timeout: keep waiting until the deadline
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(
            text.contains(": ping"),
            "空闲流应在 ~2s 内收到 keepalive，实际 {text:?}"
        );
        assert!(!text.contains("event: task_event"), "游标之后不应有事件帧");
        drop(stream);
        shutdown.store(true, Ordering::SeqCst);
    }

    /// The JSON events endpoint replays identically on repeat reads.
    #[test]
    fn events_json_replay_is_idempotent() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let (a, _) = seed(&env.ctx);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (status, _, first) = raw(
            addr,
            "GET",
            &format!("/v1/tasks/{a}/events?after=0"),
            &auth_refs,
            "",
        );
        assert_eq!(status, 200);
        let (status, _, second) = raw(
            addr,
            "GET",
            &format!("/v1/tasks/{a}/events?after=0"),
            &auth_refs,
            "",
        );
        assert_eq!(status, 200);
        assert_eq!(first, second, "两次重放应得到相同的 JSON");
        let v: Value = serde_json::from_str(&first).unwrap();
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .map(|e| e["seq"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let (status, _, body) = raw(
            addr,
            "GET",
            &format!("/v1/tasks/{a}/events?after=2"),
            &auth_refs,
            "",
        );
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        shutdown.store(true, Ordering::SeqCst);
    }

    /// `POST /v1/tasks` — 202 + the task id, and the execution
    /// parameters are persisted (schema v20) for the resident worker.
    #[test]
    fn api_create_returns_202_and_persists_params() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let world = tempfile::TempDir::new().unwrap();
        let body = serde_json::json!({
            "kind": "meeting-brief",
            "title": "会前准备",
            "world": world.path().to_string_lossy(),
            "idempotency_key": "api-test-1",
        })
        .to_string();
        let (status, _, resp) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
        assert_eq!(status, 202);
        let v: Value = serde_json::from_str(&resp).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["duplicate"], false);
        assert_eq!(v["kind"], "meeting-brief");
        let task = TaskStore::new(&env.ctx.app.db).get(&id).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Queued);
        assert_eq!(task.source, "fixture");
        let params = crate::commands::task_run_params::RunParams::load(&env.ctx.app.db, &id)
            .unwrap()
            .expect("执行参数应已持久化");
        assert_eq!(params.kind, "meeting-brief");
        assert_eq!(params.world, Some(world.path().to_path_buf()));
        assert_eq!(params.source, "fixture");
        shutdown.store(true, Ordering::SeqCst);
    }

    /// Idempotency: same key + same content → the same task
    /// (`duplicate: true`); same key + different content → 409;
    /// no key → two identical submits are two tasks.
    #[test]
    fn api_create_idempotency() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let world = tempfile::TempDir::new().unwrap();
        let world_s = world.path().to_string_lossy().to_string();
        let body = serde_json::json!({
            "kind": "meeting-brief",
            "title": "会前准备",
            "world": world_s,
            "idempotency_key": "api-test-1",
        })
        .to_string();
        let (status, _, first) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
        assert_eq!(status, 202);
        let first: Value = serde_json::from_str(&first).unwrap();
        let first_id = first["id"].as_str().unwrap().to_string();
        let (status, _, second) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
        assert_eq!(status, 202);
        let second: Value = serde_json::from_str(&second).unwrap();
        assert_eq!(second["id"].as_str().unwrap(), first_id);
        assert_eq!(second["duplicate"], true);
        assert_eq!(
            TaskStore::new(&env.ctx.app.db)
                .list(None, 100)
                .unwrap()
                .len(),
            1
        );
        let changed = serde_json::json!({
            "kind": "meeting-brief",
            "title": "改了标题",
            "world": world_s,
            "idempotency_key": "api-test-1",
        })
        .to_string();
        let (status, _, body) = raw(addr, "POST", "/v1/tasks", &auth_refs, &changed);
        assert_eq!(status, 409);
        assert!(body.contains(&first_id));
        assert_eq!(
            TaskStore::new(&env.ctx.app.db)
                .list(None, 100)
                .unwrap()
                .len(),
            1
        );
        let no_key = serde_json::json!({
            "kind": "meeting-brief",
            "title": "无幂等键",
            "world": world_s,
        })
        .to_string();
        let (status, _, v1) = raw(addr, "POST", "/v1/tasks", &auth_refs, &no_key);
        assert_eq!(status, 202);
        let (status, _, v2) = raw(addr, "POST", "/v1/tasks", &auth_refs, &no_key);
        assert_eq!(status, 202);
        let v1: Value = serde_json::from_str(&v1).unwrap();
        let v2: Value = serde_json::from_str(&v2).unwrap();
        assert_ne!(v1["id"].as_str().unwrap(), v2["id"].as_str().unwrap());
        shutdown.store(true, Ordering::SeqCst);
    }

    /// Validation: unknown kind, missing world, and a world dir
    /// that does not exist are all 400s.
    #[test]
    fn api_create_validation_rejects_bad_input() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/tasks",
            &auth_refs,
            r#"{"kind": "not-a-kind", "world": "/tmp"}"#,
        );
        assert_eq!(status, 400);
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/tasks",
            &auth_refs,
            r#"{"kind": "meeting-brief"}"#,
        );
        assert_eq!(status, 400);
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/tasks",
            &auth_refs,
            r#"{"kind": "meeting-brief", "world": "/does/not/exist"}"#,
        );
        assert_eq!(status, 400);
        shutdown.store(true, Ordering::SeqCst);
    }

    /// With the task-9 new-task entry closed, API create is refused
    /// but reads stay open; reopening lets the same body succeed.
    #[test]
    fn api_create_flag_off_refuses_but_reads_stay_open() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        hatch_release::FlagStore::new(&env.ctx.app.db)
            .set(hatch_release::NEW_TASK_ENTRY, false, &now())
            .unwrap();
        let world = tempfile::TempDir::new().unwrap();
        let body = serde_json::json!({
            "kind": "meeting-brief",
            "title": "会前准备",
            "world": world.path().to_string_lossy(),
        })
        .to_string();
        let (status, _, resp) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
        assert_eq!(status, 400);
        assert!(resp.contains("新任务入口已关闭"));
        assert!(TaskStore::new(&env.ctx.app.db)
            .list(None, 100)
            .unwrap()
            .is_empty());
        let (status, _, _) = raw(addr, "GET", "/v1/tasks", &auth_refs, "");
        assert_eq!(status, 200);
        hatch_release::FlagStore::new(&env.ctx.app.db)
            .set(hatch_release::NEW_TASK_ENTRY, true, &now())
            .unwrap();
        let (status, _, _) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
        assert_eq!(status, 202);
        shutdown.store(true, Ordering::SeqCst);
    }

    /// Pause / resume / command over HTTP, on the same ledger the
    /// CLI reads: pause → blocked, resume → queued, command with
    /// the current version bumps it, a stale one is refused.
    #[test]
    fn api_pause_resume_command_roundtrip() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let world = tempfile::TempDir::new().unwrap();
        let body = serde_json::json!({
            "kind": "meeting-brief",
            "title": "会前准备",
            "world": world.path().to_string_lossy(),
        })
        .to_string();
        let (status, _, resp) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
        assert_eq!(status, 202);
        let id = serde_json::from_str::<Value>(&resp).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, _, _) = raw(
            addr,
            "POST",
            &format!("/v1/tasks/{id}/pause"),
            &auth_refs,
            "{}",
        );
        assert_eq!(status, 200);
        assert_eq!(
            TaskStore::new(&env.ctx.app.db)
                .get(&id)
                .unwrap()
                .unwrap()
                .status,
            TaskStatus::Blocked
        );
        let (status, _, _) = raw(
            addr,
            "POST",
            &format!("/v1/tasks/{id}/resume"),
            &auth_refs,
            "{}",
        );
        assert_eq!(status, 200);
        assert_eq!(
            TaskStore::new(&env.ctx.app.db)
                .get(&id)
                .unwrap()
                .unwrap()
                .status,
            TaskStatus::Queued
        );
        let (status, _, resp) = raw(
            addr,
            "POST",
            &format!("/v1/tasks/{id}/command"),
            &auth_refs,
            r#"{"expected_version": 1, "summary": "把标题改成周报"}"#,
        );
        assert_eq!(status, 200);
        assert!(resp.contains("v2"));
        let (status, _, _) = raw(
            addr,
            "POST",
            &format!("/v1/tasks/{id}/command"),
            &auth_refs,
            r#"{"expected_version": 1, "summary": "过期的指令"}"#,
        );
        assert_eq!(status, 400);
        let (status, _, resp) = raw(addr, "GET", &format!("/v1/tasks/{id}"), &auth_refs, "");
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["plan_version"], 2);
        shutdown.store(true, Ordering::SeqCst);
    }
}
