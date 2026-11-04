//! Scheduled-job endpoints: list, create, get, run once, run
//! history, patch (enable/disable or reschedule), delete.
//!
//! Creation mirrors `hatch jobs add` exactly: same validation
//! (non-empty name/kind, unique name, schedule that fires) and the
//! same conservative defaults. `kind` must be a registered engine
//! kind — a job that could never run is rejected up front.

use super::{json_body, opt_bool, opt_str, opt_u64, ApiCtx, ApiError};
use crate::sched;
use hatch_core::time::now;
use hatch_core::CancelToken;
use hatch_mcp::http_proto::HttpRequest;
use hatch_scheduler::job::{CatchUp, Delivery, NewJob};
use hatch_scheduler::JobStore;
use serde_json::{json, Value};

pub fn list(ctx: &ApiCtx) -> Result<(u16, Value), ApiError> {
    let jobs = JobStore::new(&ctx.app.db).list()?;
    Ok((
        200,
        json!({"jobs": serde_json::to_value(&jobs).unwrap_or(Value::Null)}),
    ))
}

pub fn create(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let name = super::req_str(&body, "name")?;
    let kind = super::req_str(&body, "kind")?;
    let schedule = super::req_str(&body, "schedule")?;
    let engine = sched::engine(&ctx.app)?;
    let kinds = engine.kinds();
    if !kinds.iter().any(|k| k == &kind) {
        return Err(ApiError::bad_request(format!(
            "未知的任务类型 {kind}（可选：{}）",
            kinds.join(", ")
        )));
    }
    let mut job = NewJob::new(&name, &kind, &schedule);
    job.params = body
        .get("params")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    job.scope = super::opt_str_list(&body, "scope");
    if let Some(o) = opt_str(&body, "owner") {
        job.owner = o;
    }
    if let Some(e) = opt_bool(&body, "enabled") {
        job.enabled = e;
    }
    if let Some(d) = opt_str(&body, "delivery") {
        job.delivery = Delivery::parse(&d).map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    if let Some(d) = opt_str(&body, "done_condition") {
        job.done_condition = d;
    }
    if let Some(c) = opt_str(&body, "catch_up") {
        job.catch_up = CatchUp::parse(&c).map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    if let Some(m) = opt_u64(&body, "max_failures") {
        job.max_failures =
            u32::try_from(m).map_err(|_| ApiError::bad_request("max_failures 太大"))?;
    }
    if let Some(t) = opt_u64(&body, "timeout_secs") {
        job.timeout_secs = t;
    }
    let created = JobStore::new(&ctx.app.db).add(&job, &now(), &engine.tz())?;
    Ok((201, serde_json::to_value(&created).unwrap_or(Value::Null)))
}

pub fn get(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let job = JobStore::new(&ctx.app.db).get(id)?;
    Ok((200, serde_json::to_value(&job).unwrap_or(Value::Null)))
}

pub fn run_now(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let result = sched::engine(&ctx.app)?.run_now(id, &now(), &CancelToken::new())?;
    Ok((200, serde_json::to_value(&result).unwrap_or(Value::Null)))
}

pub fn runs(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let limit = req
        .query_param("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20usize)
        .clamp(1, 100);
    let runs = JobStore::new(&ctx.app.db).history(Some(id), limit)?;
    Ok((
        200,
        json!({"job_id": id, "runs": serde_json::to_value(&runs).unwrap_or(Value::Null)}),
    ))
}

/// `PATCH /v1/jobs/{id}` — body `{"enabled": bool, "schedule": "..."}`
/// (either or both). Rescheduling re-anchors the next firing at now.
pub fn patch(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let store = JobStore::new(&ctx.app.db);
    let tz = sched::engine(&ctx.app)?.tz();
    let now = now();
    if let Some(s) = opt_str(&body, "schedule") {
        store.set_schedule(id, &s, &now, &tz)?;
    }
    if let Some(e) = opt_bool(&body, "enabled") {
        store.set_enabled(id, e, "api", &now, &tz)?;
    }
    let job = store.get(id)?;
    Ok((200, serde_json::to_value(&job).unwrap_or(Value::Null)))
}

pub fn remove(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    JobStore::new(&ctx.app.db).remove(id)?;
    Ok((200, json!({"id": id, "deleted": true})))
}
