//! Route table for the product API.
//!
//! One match arm per endpoint; handlers live in the resource modules
//! ([`super::sessions`], [`super::memory`], …). A path that matches a
//! known route shape with the wrong method gets 405 instead of 404.

use super::{chat, json_body, ApiCtx, ApiError};
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
        ("POST", ["v1", "sessions", id, "messages"]) => {
            let body = json_body(req)?;
            chat::run_turn_json(ctx, id, &body)
        }
        ("POST", ["v1", "sessions", id, "cancel"]) => chat::cancel_turn(ctx, id),
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
            | ["v1", "artifacts", _]
            | ["v1", "artifacts", _, "share"]
            | ["v1", "artifacts", _, "revoke"]
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
