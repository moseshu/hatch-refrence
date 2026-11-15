//! Task write endpoints (P0 product closed loop): create and the
//! control plane for external clients — `POST /v1/tasks`,
//! `POST /v1/tasks/{id}/pause`, `/resume`, `/command`.
//!
//! The read endpoints live in [`super::tasks`]; the write side is
//! split out so neither file grows past the 400-line rule.

use super::{json_body, opt_str, req_str, ApiCtx, ApiError};
use crate::commands::task_control::{apply_command, pause_task, resume_task};
use crate::commands::task_run_params::RunParams;
use hatch_core::time::now;
use hatch_mcp::http_proto::HttpRequest;
use hatch_tasks::{CreateOutcome, TaskStatus, TaskStore};
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
