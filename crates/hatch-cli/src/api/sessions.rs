//! Session endpoints: create, list, get, fork, rewind, delete.
//!
//! Sessions are the unit of a chat: `SessionStore::active` returns the
//! latest summary plus the uncompacted messages, so a summary written
//! by the background compaction worker shows up here automatically.

use super::{json_body, opt_i64, opt_str, ApiCtx, ApiError};
use hatch_agent::SessionStore;
use hatch_core::time::to_rfc3339;
use hatch_mcp::http_proto::HttpRequest;
use serde_json::{json, Value};

/// `GET /v1/sessions` (query `limit`, default 20) and
/// `POST /v1/sessions` (body `{"title": ...}`).
pub fn create(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let title = opt_str(&body, "title").unwrap_or_else(|| "API 会话".to_string());
    let now = hatch_core::time::now();
    let id = SessionStore::new(&ctx.app.db).create(&title, &now)?;
    Ok((
        201,
        json!({"id": id, "title": title, "created_at": to_rfc3339(&now), "messages": 0}),
    ))
}

pub fn list(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let limit = req
        .query_param("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20usize)
        .clamp(1, 100);
    let sessions = SessionStore::new(&ctx.app.db).list(false, limit)?;
    Ok((
        200,
        json!({"sessions": sessions.iter().map(|s| json!({
            "id": s.id, "title": s.title, "created_at": s.created_at, "messages": s.messages,
        })).collect::<Vec<_>>()}),
    ))
}

fn require(ctx: &ApiCtx, id: &str) -> Result<(), ApiError> {
    if !SessionStore::new(&ctx.app.db).exists(id)? {
        return Err(ApiError::not_found("会话"));
    }
    Ok(())
}

pub fn get(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = SessionStore::new(&ctx.app.db);
    let info = store.info(id)?.ok_or_else(|| ApiError::not_found("会话"))?;
    let (summary, history) = store.active(id)?;
    Ok((
        200,
        json!({
            "id": info.id,
            "title": info.title,
            "created_at": info.created_at,
            "summary": summary,
            "messages": history.iter().map(|m| json!({
                "seq": m.seq,
                "message": serde_json::to_value(&m.message).unwrap_or(Value::Null),
                "trust": serde_json::to_value(m.trust).unwrap_or(Value::Null),
            })).collect::<Vec<_>>(),
        }),
    ))
}

pub fn fork(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = SessionStore::new(&ctx.app.db);
    let now = hatch_core::time::now();
    let new_id = store.fork(id, &now)?;
    let info = store
        .info(&new_id)?
        .ok_or_else(|| ApiError::not_found("会话"))?;
    Ok((
        201,
        json!({"id": info.id, "title": info.title, "created_at": info.created_at}),
    ))
}

pub fn rewind(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let to =
        opt_i64(&body, "to_seq").ok_or_else(|| ApiError::bad_request("缺少字段 to_seq（整数）"))?;
    let report = SessionStore::new(&ctx.app.db).rewind(id, to)?;
    Ok((
        200,
        json!({"id": id, "removed": report.removed, "uncompacted": report.uncompacted}),
    ))
}

pub fn delete(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    if ctx.running.lock().unwrap().contains_key(id) {
        return Err(ApiError::conflict("该会话有进行中的回合，无法删除"));
    }
    let deleted = SessionStore::new(&ctx.app.db).delete(id)?;
    if !deleted {
        return Err(ApiError::not_found("会话"));
    }
    Ok((200, json!({"id": id, "deleted": true})))
}

/// For other handlers: 404 when the session does not exist.
pub fn require_session(ctx: &ApiCtx, id: &str) -> Result<(), ApiError> {
    require(ctx, id)
}
