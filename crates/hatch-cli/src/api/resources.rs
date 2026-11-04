//! Read-mostly resources: skills, connectors, goals, feed, ideas,
//! calendar events and the mail triage receipt. (Artifact endpoints
//! live in [`super::artifacts`].)
//!
//! Every handler mirrors the matching CLI command: same store, same
//! validation, same defaults. One deliberate limit:
//! `GET /v1/connectors` never spawns a connector: it reports the
//! manifest plus which env vars are missing. The live check
//! (`GET /v1/connectors/{name}`) runs the connector's `status` op,
//! like `hatch connectors status` — an explicit per-connector
//! action, not a list side effect.

use super::{json_body, req_str, ApiCtx, ApiError};
use chrono::Duration;
use hatch_connectors::framework::{self, ConnectorManifest};
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_connectors::CalendarRepo;
use hatch_core::time::now;
use hatch_goals::{goal_attention, FeedStore, GoalStatus, GoalStore, IdeaStore};
use hatch_mcp::http_proto::HttpRequest;
use hatch_skills::registry::active_skills;
use serde_json::{json, Value};
/// `GET /v1/skills` — installed skills with their allowlist and
/// whether each one is active for future chat turns.
pub fn skills_list(ctx: &ApiCtx) -> Result<(u16, Value), ApiError> {
    let reg = ctx.app.skill_registry();
    let active = active_skills(&ctx.app.home);
    let skills = reg.scan()?;
    Ok((
        200,
        json!({"skills": skills.iter().map(|s| json!({
            "name": s.name,
            "description": s.description,
            "tools": s.tools,
            "scope": s.scope,
            "version": s.version,
            "active": active.iter().any(|a| a == &s.name),
        })).collect::<Vec<_>>()}),
    ))
}

fn connector_json(m: &ConnectorManifest, missing_env: &[String]) -> Value {
    json!({
        "name": m.name,
        "command": m.command,
        "token_ref": m.token_ref,
        "timeout_secs": m.timeout().as_secs(),
        "ops": m.ops.iter().map(|o| json!({
            "name": o.name, "description": o.description, "read_only": o.read_only,
        })).collect::<Vec<_>>(),
        "missing_env": missing_env,
    })
}

/// `GET /v1/connectors` — manifests plus missing env vars. No
/// subprocess is spawned here; see [`connector_status`].
pub fn connectors_list(ctx: &ApiCtx) -> Result<(u16, Value), ApiError> {
    let (manifests, warnings) = framework::load_dir(&ctx.app.connectors_dir());
    let items: Vec<Value> = manifests
        .iter()
        .map(|m| {
            let (_, missing) = framework::resolve_env(m);
            connector_json(m, &missing)
        })
        .collect();
    Ok((200, json!({"connectors": items, "warnings": warnings})))
}

/// `GET /v1/connectors/{name}` — the live status check: runs the
/// connector's `status` op when it declares one.
pub fn connector_status(ctx: &ApiCtx, name: &str) -> Result<(u16, Value), ApiError> {
    let manifest = framework::find(&ctx.app.connectors_dir(), name)?;
    let (env, missing) = framework::resolve_env(&manifest);
    let check = framework::status(&manifest, &env);
    let mut v = connector_json(&manifest, &missing);
    if let Value::Object(ref mut map) = v {
        map.insert(
            "status".into(),
            json!({"ok": check.ok, "detail": check.detail, "guidance": check.guidance}),
        );
    }
    Ok((200, v))
}

fn goal_json(ctx: &ApiCtx, g: &hatch_goals::Goal) -> Value {
    let store = GoalStore::new(&ctx.app.home);
    let attention = goal_attention(&ctx.app.db, &store, &g.id, &now())
        .map(|a| a.as_str().to_string())
        .unwrap_or_else(|_| "—".into());
    json!({
        "id": g.id,
        "title": g.title,
        "status": g.status.as_str(),
        "created_at": g.created_at,
        "updated_at": g.updated_at,
        "attention": attention,
    })
}

/// `GET /v1/goals` — active + paused by default; `?all=true`
/// lists everything.
pub fn goals_list(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let all = req
        .query_param("all")
        .is_some_and(|v| v == "true" || v == "1");
    let store = GoalStore::new(&ctx.app.home);
    let mut goals = store.list()?;
    if !all {
        goals.retain(|g| g.status == GoalStatus::Active || g.status == GoalStatus::Paused);
    }
    Ok((
        200,
        json!({"goals": goals.iter().map(|g| goal_json(ctx, g)).collect::<Vec<_>>()}),
    ))
}

/// `POST /v1/goals` — body `{"title"}`.
pub fn goals_create(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let title = req_str(&body, "title")?;
    let g = GoalStore::new(&ctx.app.home).create(&title, &now())?;
    Ok((201, goal_json(ctx, &g)))
}

/// `GET /v1/goals/{id}` — the goal plus its GOAL.md body and the
/// supporting files under `files/`.
pub fn goal_get(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = GoalStore::new(&ctx.app.home);
    // The store reports a missing goal as Invalid (400); the API
    // contract is 404 for a missing resource, so check first.
    if !store.dir(id).join("GOAL.md").exists() {
        return Err(ApiError::not_found("目标"));
    }
    let g = store.get(id)?;
    let mut v = goal_json(ctx, &g);
    if let Value::Object(ref mut map) = v {
        map.insert("body".into(), json!(g.body));
        map.insert("files".into(), json!(store.files(id).unwrap_or_default()));
    }
    Ok((200, v))
}

/// `GET /v1/feed?limit=` — recent feed items (newest first).
pub fn feed_list(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let limit = req
        .query_param("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10usize)
        .clamp(1, 100);
    let items = FeedStore::new(&ctx.app.db).list(limit)?;
    Ok((
        200,
        json!({"feed": serde_json::to_value(&items).unwrap_or(Value::Null)}),
    ))
}

/// `GET /v1/ideas?limit=` — unprocessed/acted-on ideas; dismissed
/// ones never resurface (the store excludes them by construction).
pub fn ideas_list(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let limit = req
        .query_param("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20usize)
        .clamp(1, 100);
    let items = IdeaStore::new(&ctx.app.db).list(limit)?;
    Ok((
        200,
        json!({"ideas": serde_json::to_value(&items).unwrap_or(Value::Null)}),
    ))
}

/// `GET /v1/calendar/events?days=` — local events in the next
/// `days` days (default 7), from the CalDAV-synced store.
pub fn calendar_events(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let days = req
        .query_param("days")
        .and_then(|v| v.parse().ok())
        .unwrap_or(7i64)
        .clamp(1, 90);
    let now = now();
    let events = CalendarRepo::new(&ctx.app.db).between(&now, &(now + Duration::days(days)))?;
    Ok((
        200,
        json!({"events": serde_json::to_value(&events).unwrap_or(Value::Null)}),
    ))
}

/// `GET /v1/mail/triage` — the last triage receipt (or null when no
/// run has happened yet) plus the outbox drafts waiting to be sent.
/// Read-only: sending always goes through the approval queue.
pub fn mail_triage(ctx: &ApiCtx) -> Result<(u16, Value), ApiError> {
    let receipt = match hatch_triage::repo::last_receipt(&ctx.app.db)? {
        Some(text) => serde_json::from_str::<Value>(&text)?,
        None => Value::Null,
    };
    let drafts = FileOutbox::new(&ctx.app.outbox_dir(), &ctx.app.user_email())?.entries()?;
    Ok((
        200,
        json!({
            "receipt": receipt,
            "drafts": serde_json::to_value(&drafts).unwrap_or(Value::Null),
        }),
    ))
}
