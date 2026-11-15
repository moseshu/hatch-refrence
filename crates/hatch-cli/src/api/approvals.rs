//! Approval endpoints: list the pending queue, show one approval,
//! approve (and execute through the same path as the CLI) or deny.
//!
//! Approving executes the queued payload exactly like
//! `hatch approvals approve` — same executors, same ledger, same
//! idempotency. An approval that is no longer pending is never
//! executed twice.

use super::{json_body, opt_bool, opt_str, ApiCtx, ApiError};
use crate::commands::approval_service::{ApprovalService, ServiceOutcome};
use hatch_core::time::now;
use hatch_mcp::http_proto::HttpRequest;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use serde_json::{json, Value};

fn approval_json(a: &hatch_policy::Approval) -> Value {
    serde_json::to_value(a).unwrap_or(Value::Null)
}

/// `GET /v1/approvals` — pending by default; `?all=true` lists
/// everything, `?origin=chat:...` filters by origin.
pub fn list(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let q = ApprovalQueue::new(&ctx.app.db);
    q.expire_due(&now())?;
    let all = req
        .query_param("all")
        .is_some_and(|v| v == "true" || v == "1");
    let origin = req.query_param("origin");
    let mut items = q.list(if all {
        None
    } else {
        Some(ApprovalStatus::Pending)
    })?;
    if let Some(o) = &origin {
        items.retain(|a| a.origin == *o);
    }
    Ok((
        200,
        json!({"approvals": items.iter().map(approval_json).collect::<Vec<_>>()}),
    ))
}

/// `GET /v1/approvals/{id}` — the exact payload that would execute,
/// plus whether it is still intact.
pub fn show(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let a = ApprovalQueue::new(&ctx.app.db).get(id)?;
    let mut v = approval_json(&a);
    if let Value::Object(ref mut m) = v {
        m.insert("payload_intact".into(), json!(a.payload_intact()));
    }
    Ok((200, v))
}

/// `POST /v1/approvals/{id}/approve` — body `{"note"?, "dry_run"?}`.
/// Approves and executes; the outcome tells the client what happened.
/// Runs through the shared [`ApprovalService`], so the HTTP path
/// gets the same plan-version check, cancellation guard and Gmail
/// sender selection as the CLI. A successful decision wakes the
/// resident worker: the task the approval waited on can continue
/// without a manual `task run`.
pub fn approve(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let note = opt_str(&body, "note").unwrap_or_default();
    let dry_run = opt_bool(&body, "dry_run").unwrap_or(false);
    match ApprovalService::new(&ctx.app).approve_and_execute(id, &note, dry_run, &now())? {
        ServiceOutcome::Refused(reason) => Err(ApiError::conflict(reason)),
        outcome => {
            crate::commands::task_resident::wake(&ctx.app);
            Ok((
                200,
                json!({"id": id, "status": "approved", "outcome": outcome.outcome_str(), "result": outcome.detail()}),
            ))
        }
    }
}

/// `POST /v1/approvals/{id}/deny` — body `{"note"?}`. A denial also
/// wakes the resident worker: the waiting step fails with the reason
/// on the next run instead of sitting blocked forever.
pub fn deny(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let note = opt_str(&body, "note").unwrap_or_default();
    let q = ApprovalQueue::new(&ctx.app.db);
    let current = q.get(id)?.status;
    if current != ApprovalStatus::Pending {
        return Err(ApiError::conflict(format!(
            "审批 {id} 当前状态为 {}，无需再次决定",
            current.as_str()
        )));
    }
    let s = q.decide(id, false, &note, &now())?;
    crate::commands::task_resident::wake(&ctx.app);
    Ok((200, json!({"id": id, "status": s.as_str()})))
}
