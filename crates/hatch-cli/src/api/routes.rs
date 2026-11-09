//! Route table for the product API.
//!
//! One match arm per endpoint; handlers live in the resource modules
//! ([`super::sessions`], [`super::memory`], …). A path that matches a
//! known route shape with the wrong method gets 405 instead of 404.

use super::{
    agents, approvals, artifacts, chat, inference, jobs, json_body, memory, resources, sessions, ApiCtx, ApiError,
};
use hatch_mcp::http_proto::HttpRequest;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_scheduler::JobStore;
use serde_json::{json, Value};

/// The session id when this request is an SSE chat turn:
/// `POST /v1/sessions/{id}/messages` with
/// `Accept: text/event-stream`. The server hands the stream to the
/// chat handler instead of the JSON dispatcher.
pub fn chat_stream_session(req: &HttpRequest) -> Option<String> {
    if req.method != "POST" {
        return None;
    }
    let accept = req.header("accept").unwrap_or("");
    if !accept.contains("text/event-stream") {
        return None;
    }
    let segments: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["v1", "sessions", id, "messages"] => Some(id.to_string()),
        _ => None,
    }
}

/// Route one JSON request. Returns the HTTP status and body.
pub fn dispatch_json(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let segments: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
    let m = req.method.as_str();
    match (m, segments.as_slice()) {
        ("GET", ["v1", "health"]) => Ok((200, json!({"status": "ok"}))),
        ("GET", ["v1", "status"]) => status(ctx),
        ("POST", ["v1", "sessions"]) => sessions::create(ctx, req),
        ("GET", ["v1", "sessions"]) => sessions::list(ctx, req),
        ("GET", ["v1", "sessions", id]) => sessions::get(ctx, id),
        ("DELETE", ["v1", "sessions", id]) => sessions::delete(ctx, id),
        ("POST", ["v1", "sessions", id, "messages"]) => {
            let body = json_body(req)?;
            chat::run_turn_json(ctx, id, &body)
        }
        ("POST", ["v1", "sessions", id, "cancel"]) => chat::cancel_turn(ctx, id),
        ("GET", ["v1", "agents"]) => agents::list(ctx, req),
        ("GET", ["v1", "agents", id]) => agents::get(ctx, id),
        ("POST", ["v1", "agents", id, "cancel"]) => agents::cancel(ctx, id),
        ("POST", ["v1", "agents", id, "pause"]) => agents::pause(ctx, id),
        ("POST", ["v1", "agents", id, "resume"]) => agents::resume(ctx, id),
        ("POST", ["v1", "sessions", id, "fork"]) => sessions::fork(ctx, id),
        ("POST", ["v1", "sessions", id, "rewind"]) => sessions::rewind(ctx, req, id),
        ("GET", ["v1", "memory", "search"]) => memory::search(ctx, req),
        ("POST", ["v1", "memory", "claims"]) => memory::add(ctx, req),
        ("GET", ["v1", "memory", "claims", id]) => memory::explain(ctx, id),
        ("GET", ["v1", "memory", "people"]) => memory::people(ctx),
        ("GET", ["v1", "memory", "people", email]) => memory::person(ctx, email),
        ("GET", ["v1", "approvals"]) => approvals::list(ctx, req),
        ("GET", ["v1", "approvals", id]) => approvals::show(ctx, id),
        ("POST", ["v1", "approvals", id, "approve"]) => approvals::approve(ctx, req, id),
        ("POST", ["v1", "approvals", id, "deny"]) => approvals::deny(ctx, req, id),
        ("GET", ["v1", "jobs"]) => jobs::list(ctx),
        ("POST", ["v1", "jobs"]) => jobs::create(ctx, req),
        ("GET", ["v1", "jobs", id]) => jobs::get(ctx, id),
        ("PATCH", ["v1", "jobs", id]) => jobs::patch(ctx, req, id),
        ("DELETE", ["v1", "jobs", id]) => jobs::remove(ctx, id),
        ("POST", ["v1", "jobs", id, "run"]) => jobs::run_now(ctx, id),
        ("GET", ["v1", "jobs", id, "runs"]) => jobs::runs(ctx, req, id),
        ("POST", ["v1", "artifacts"]) => artifacts::artifact_create(ctx, req),
        // The literal "quota" arm must come before the `{id}` arm,
        // or "quota" would match as an artifact id.
        ("GET", ["v1", "artifacts", "quota"]) => artifacts::artifact_quota(ctx),
        ("GET", ["v1", "artifacts", id]) => artifacts::artifact_get(ctx, req, id),
        ("DELETE", ["v1", "artifacts", id]) => artifacts::artifact_delete(ctx, id),
        ("GET", ["v1", "artifacts", id, "shares"]) => artifacts::artifact_shares(ctx, id),
        ("POST", ["v1", "artifacts", id, "share"]) => artifacts::artifact_share(ctx, id),
        ("POST", ["v1", "artifacts", id, "revoke"]) => artifacts::artifact_revoke(ctx, id),
        ("GET", ["v1", "shared", token]) => artifacts::artifact_shared(ctx, token),
        ("GET", ["v1", "inference", "usage"]) => inference::usage(ctx, req),
        ("GET", ["v1", "skills"]) => resources::skills_list(ctx),
        ("GET", ["v1", "connectors"]) => resources::connectors_list(ctx),
        ("GET", ["v1", "connectors", name]) => resources::connector_status(ctx, name),
        ("GET", ["v1", "goals"]) => resources::goals_list(ctx, req),
        ("POST", ["v1", "goals"]) => resources::goals_create(ctx, req),
        ("GET", ["v1", "goals", id]) => resources::goal_get(ctx, id),
        ("GET", ["v1", "feed"]) => resources::feed_list(ctx, req),
        ("GET", ["v1", "ideas"]) => resources::ideas_list(ctx, req),
        ("GET", ["v1", "calendar", "events"]) => resources::calendar_events(ctx, req),
        ("GET", ["v1", "mail", "triage"]) => resources::mail_triage(ctx),
        _ => {
            if known_path(&segments) {
                Err(ApiError::new(
                    405,
                    "method_not_allowed",
                    format!("{m} 不适用于 {}", req.path),
                ))
            } else {
                Err(ApiError::not_found("接口"))
            }
        }
    }
}

/// Path shapes the API knows (ignoring method), for 405 detection.
fn known_path(segments: &[&str]) -> bool {
    matches!(
        segments,
        ["v1", "health"]
            | ["v1", "status"]
            | ["v1", "sessions"]
            | ["v1", "sessions", _, "messages"]
            | ["v1", "sessions", _, "cancel"]
            | ["v1", "agents"]
            | ["v1", "agents", _]
            | ["v1", "agents", _, "cancel"]
            | ["v1", "agents", _, "pause"]
            | ["v1", "agents", _, "resume"]
            | ["v1", "sessions", _, "fork"]
            | ["v1", "sessions", _, "rewind"]
            | ["v1", "sessions", _]
            | ["v1", "memory", "search"]
            | ["v1", "memory", "claims"]
            | ["v1", "memory", "claims", _]
            | ["v1", "memory", "people"]
            | ["v1", "memory", "people", _]
            | ["v1", "approvals"]
            | ["v1", "approvals", _]
            | ["v1", "approvals", _, "approve"]
            | ["v1", "approvals", _, "deny"]
            | ["v1", "jobs"]
            | ["v1", "jobs", _]
            | ["v1", "jobs", _, "run"]
            | ["v1", "jobs", _, "runs"]
            | ["v1", "artifacts"]
            | ["v1", "artifacts", "quota"]
            | ["v1", "artifacts", _]
            | ["v1", "artifacts", _, "shares"]
            | ["v1", "artifacts", _, "share"]
            | ["v1", "artifacts", _, "revoke"]
            | ["v1", "shared", _]
            | ["v1", "inference", "usage"]
            | ["v1", "skills"]
            | ["v1", "connectors"]
            | ["v1", "connectors", _]
            | ["v1", "goals"]
            | ["v1", "goals", _]
            | ["v1", "feed"]
            | ["v1", "ideas"]
            | ["v1", "calendar", "events"]
            | ["v1", "mail", "triage"]
    )
}

/// `GET /v1/status`: daemon state, model wiring, queue depths.
fn status(ctx: &ApiCtx) -> Result<(u16, Value), ApiError> {
    let db = &ctx.app.db;
    let meta = |k: &str| db.meta(k).ok().flatten().unwrap_or_default();
    let enabled_jobs = JobStore::new(db)
        .list()
        .map(|jobs| jobs.iter().filter(|j| j.enabled).count())
        .unwrap_or(0);
    let pending = ApprovalQueue::new(db)
        .list(Some(ApprovalStatus::Pending))
        .map(|a| a.len())
        .unwrap_or(0);
    Ok((
        200,
        json!({
            "daemon": meta("daemon_state"),
            "started_at": meta("daemon_started_at"),
            "last_tick": meta("daemon_last_tick"),
            "model_configured": !ctx.app.config.resolved_models().is_empty(),
            "enabled_jobs": enabled_jobs,
            "pending_approvals": pending,
        }),
    ))
}
