//! Sub-agent run endpoints: list, show, cancel, pause, resume by run id.
//!
//! Cancellation goes through the daemon process's registry: it owns
//! the child processes (and their kill switches). A run that is still
//! open but lives in another process answers 409. Pausing and resuming
//! go through the same registry (D5): pausing freezes the child in
//! place, resuming continues the same run.

use super::{ApiCtx, ApiError};
use hatch_agent::{ControlOutcome, RunRecord, RunStatus, RunStore};
use hatch_mcp::http_proto::HttpRequest;
use serde_json::{json, Value};

fn run_json(r: &RunRecord) -> Value {
    json!({
        "id": r.id,
        "parent": r.parent,
        "session": r.session,
        "depth": r.depth,
        "agent_type": r.agent_type,
        "task": r.task,
        "status": r.status.as_str(),
        "started_at": r.started_at,
        "finished_at": r.finished_at,
        "steps": r.steps,
        "answer": r.answer,
        "error": r.error,
        "model_calls": r.model_calls,
        "prompt_tokens": r.prompt_tokens,
        "completion_tokens": r.completion_tokens,
        "child_pid": r.child_pid,
    })
}

/// `GET /v1/agents` (query `session`, `limit`, default 20).
pub fn list(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let session = req.query_param("session");
    let limit = req
        .query_param("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20usize)
        .clamp(1, 100);
    let runs = RunStore::new(&ctx.app.db).list(session.as_deref(), limit)?;
    Ok((200, json!({"agents": runs.iter().map(run_json).collect::<Vec<_>>()})))
}

/// `GET /v1/agents/{id}`: the run, its progress events and its handoff.
pub fn get(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = RunStore::new(&ctx.app.db);
    let run = store.get(id)?.ok_or_else(|| ApiError::not_found("子代理运行"))?;
    let events = store.events(id)?;
    let handoff = store
        .get_handoff(id)?
        .map(|(text, tainted)| json!({"text": text, "tainted": tainted}));
    let mut body = run_json(&run);
    body["events"] = json!(events
        .iter()
        .map(|e| json!({"seq": e.seq, "ts": e.ts, "kind": e.kind, "detail": e.detail}))
        .collect::<Vec<_>>());
    body["handoff"] = handoff.unwrap_or(Value::Null);
    Ok((200, body))
}

/// `POST /v1/agents/{id}/cancel`: kill the child, record cancelled.
/// The kill itself is asynchronous (the monitor loop reaps the child
/// within ~200 ms); 200 with `cancelled: true` means the cancel was
/// accepted. 409 when the run is still open but owned by another
/// process; 200 with `cancelled: false` when it already finished.
pub fn cancel(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = RunStore::new(&ctx.app.db);
    let run = store.get(id)?.ok_or_else(|| ApiError::not_found("子代理运行"))?;
    if ctx.app.subagent_registry.cancel(id) {
        let _ = ctx.app.db.audit("subagent", "cancelled", &format!("{id}: API 取消"));
        return Ok((200, json!({"id": id, "cancelled": true, "status": "cancelled"})));
    }
    match run.status {
        RunStatus::Spawned | RunStatus::Running => Err(ApiError::new(
            409,
            "not_owned",
            "该子代理不在本进程运行，无法取消（可能属于另一个进程，或已随父进程退出）。",
        )),
        _ => Ok((200, json!({"id": id, "cancelled": false, "status": run.status.as_str()}))),
    }
}

/// `POST /v1/agents/{id}/pause`: freeze the child in place (D5).
/// 200 with `paused: true` means the run is paused now — including a
/// repeat pause, which is an idempotent no-op. 409 when the run is
/// still open but owned by another process; 200 with `paused: false`
/// when it already finished.
pub fn pause(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = RunStore::new(&ctx.app.db);
    let run = store.get(id)?.ok_or_else(|| ApiError::not_found("子代理运行"))?;
    match ctx.app.subagent_registry.pause(id) {
        ControlOutcome::Done | ControlOutcome::Already => {
            let _ = ctx.app.db.audit("subagent", "paused", &format!("{id}: API 暂停"));
            Ok((200, json!({"id": id, "paused": true, "status": "paused"})))
        }
        ControlOutcome::Terminal(status) => Ok((200, json!({"id": id, "paused": false, "status": status.as_str()}))),
        ControlOutcome::NotOwned => match run.status {
            RunStatus::Spawned | RunStatus::Running | RunStatus::Paused => Err(ApiError::new(
                409,
                "not_owned",
                "该子代理不在本进程运行，无法暂停（可能属于另一个进程，或已随父进程退出）。",
            )),
            _ => Ok((200, json!({"id": id, "paused": false, "status": run.status.as_str()}))),
        },
    }
}

/// `POST /v1/agents/{id}/resume`: continue a paused run (D5) from
/// where the pause froze it. Same response shape as [`pause`]:
/// `resumed: true` means the run is running now, including a repeat
/// resume; 409 when it is open but owned by another process.
pub fn resume(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = RunStore::new(&ctx.app.db);
    let run = store.get(id)?.ok_or_else(|| ApiError::not_found("子代理运行"))?;
    match ctx.app.subagent_registry.resume(id) {
        ControlOutcome::Done | ControlOutcome::Already => {
            let _ = ctx.app.db.audit("subagent", "resumed", &format!("{id}: API 恢复"));
            Ok((200, json!({"id": id, "resumed": true, "status": "running"})))
        }
        ControlOutcome::Terminal(status) => Ok((200, json!({"id": id, "resumed": false, "status": status.as_str()}))),
        ControlOutcome::NotOwned => match run.status {
            RunStatus::Spawned | RunStatus::Running | RunStatus::Paused => Err(ApiError::new(
                409,
                "not_owned",
                "该子代理不在本进程运行，无法恢复（可能属于另一个进程，或已随父进程退出）。",
            )),
            _ => Ok((200, json!({"id": id, "resumed": false, "status": run.status.as_str()}))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use hatch_agent::RegistryEntry;
    use hatch_core::CancelToken;
    use hatch_mcp::http_proto::parse_request;
    use std::sync::Arc;

    fn ctx() -> (tempfile::TempDir, Arc<ApiCtx>) {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        (dir, Arc::new(ApiCtx::new(app)))
    }

    fn seed(ctx: &ApiCtx, id: &str, status: RunStatus) {
        let store = RunStore::new(&ctx.app.db);
        let now = hatch_core::time::now();
        store
            .create(id, "session:s", "s", 1, "explore", "查预算", status, &now)
            .unwrap();
    }

    #[test]
    fn list_get_and_cancel_flow() {
        let (_dir, ctx) = ctx();
        seed(&ctx, "sa_1", RunStatus::Running);
        seed(&ctx, "sa_2", RunStatus::Completed);
        let raw = b"GET /v1/agents?limit=10 HTTP/1.1\r\nHost: x\r\n\r\n";
        let req = parse_request(&mut std::io::Cursor::new(&raw[..])).unwrap();
        let (status, body) = list(&ctx, &req).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["agents"].as_array().unwrap().len(), 2);
        let (status, body) = get(&ctx, "sa_1").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["status"], "running");
        assert_eq!(body["events"].as_array().unwrap().len(), 0);
        assert!(body["handoff"].is_null());
        assert!(get(&ctx, "sa_nope").is_err(), "unknown run is 404");
    }

    #[test]
    fn cancel_owned_running_then_finished_is_idempotent() {
        let (_dir, ctx) = ctx();
        seed(&ctx, "sa_1", RunStatus::Running);
        // A live child owned by this process: register a fake entry.
        let token = CancelToken::new();
        assert!(ctx.app.subagent_registry.register(
            "sa_1",
            RegistryEntry { cancel: token.clone(), session: "s".into(), parent_ref: "session:s".into() }
        ));
        let (status, body) = cancel(&ctx, "sa_1").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["cancelled"], true);
        assert!(token.is_cancelled());
        // Already finished: cancelling again reports no-op, not an error.
        seed(&ctx, "sa_2", RunStatus::Completed);
        let (status, body) = cancel(&ctx, "sa_2").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["cancelled"], false);
        assert_eq!(body["status"], "completed");
        assert!(cancel(&ctx, "sa_nope").is_err(), "unknown run is 404");
    }

    #[test]
    fn cancel_running_but_not_owned_is_409() {
        let (_dir, ctx) = ctx();
        seed(&ctx, "sa_1", RunStatus::Running);
        // No registry entry: another process owns it (or it is stale).
        let err = cancel(&ctx, "sa_1").unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(err.code, "not_owned");
    }

    /// A live process the API can pause and resume. Killed on drop.
    struct LiveSleeper {
        child: std::process::Child,
    }

    impl Drop for LiveSleeper {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn pause_resume_flow_with_a_real_process() {
        let (_dir, ctx) = ctx();
        let mut sleeper = LiveSleeper {
            child: std::process::Command::new("sleep").arg("60").spawn().expect("sleep must exist for this test"),
        };
        let pid = sleeper.child.id();
        let store = RunStore::new(&ctx.app.db);
        let now = hatch_core::time::now();
        store.create("sa_1", "session:s", "s", 1, "explore", "查预算", RunStatus::Running, &now).unwrap();
        store.set_child_pid("sa_1", pid).unwrap();
        let token = CancelToken::new();
        assert!(ctx.app.subagent_registry.register(
            "sa_1",
            RegistryEntry { cancel: token.clone(), session: "s".into(), parent_ref: "session:s".into() }
        ));
        let (status, body) = pause(&ctx, "sa_1").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["paused"], true);
        assert_eq!(body["status"], "paused");
        assert_eq!(store.get("sa_1").unwrap().unwrap().status, RunStatus::Paused);
        // A repeat pause is an idempotent no-op.
        let (status, body) = pause(&ctx, "sa_1").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["paused"], true);
        let (status, body) = resume(&ctx, "sa_1").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["resumed"], true);
        assert_eq!(body["status"], "running");
        assert_eq!(store.get("sa_1").unwrap().unwrap().status, RunStatus::Running);
        // A repeat resume is an idempotent no-op too.
        let (status, body) = resume(&ctx, "sa_1").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["resumed"], true);
        let _ = sleeper.child.kill();
        let _ = sleeper.child.wait();
    }

    #[test]
    fn pause_resume_terminal_and_unowned() {
        let (_dir, ctx) = ctx();
        seed(&ctx, "sa_done", RunStatus::Completed);
        let (status, body) = pause(&ctx, "sa_done").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["paused"], false);
        assert_eq!(body["status"], "completed");
        let (status, body) = resume(&ctx, "sa_done").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["resumed"], false);
        assert_eq!(body["status"], "completed");
        seed(&ctx, "sa_1", RunStatus::Running);
        let err = pause(&ctx, "sa_1").unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(err.code, "not_owned");
        let err = resume(&ctx, "sa_1").unwrap_err();
        assert_eq!(err.status, 409);
        assert!(pause(&ctx, "sa_nope").is_err(), "unknown run is 404");
        assert!(resume(&ctx, "sa_nope").is_err(), "unknown run is 404");
    }
}
