//! Memory endpoints: BM25 keyword search, add a claim, explain a
//! claim (provenance), and the people/groups relationship pages.

use super::{json_body, opt_bool, opt_str, ApiCtx, ApiError};
use hatch_core::time::parse_rfc3339;
use hatch_mcp::http_proto::HttpRequest;
use hatch_memory::{MemoryStore, NewClaim};
use serde_json::{json, Value};

/// `GET /v1/memory/search?q=...&k=...` — BM25 keyword search over
/// subject + text + tags. No vector index, by design.
pub fn search(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let q = req
        .query_param("q")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("缺少查询参数 q"))?;
    let k = req
        .query_param("k")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5usize)
        .clamp(1, 20);
    let now = hatch_core::time::now();
    let hits = MemoryStore::new(&ctx.app.db).search(&q, k, &now)?;
    let results: Vec<Value> = hits
        .into_iter()
        .map(|(c, score)| {
            let mut v = serde_json::to_value(&c).unwrap_or(Value::Null);
            if let Value::Object(ref mut m) = v {
                m.insert("score".into(), json!(score));
            }
            v
        })
        .collect();
    Ok((200, json!({"query": q, "results": results})))
}

/// `POST /v1/memory/claims` — body `{"subject", "text", "tags"?,
/// "confidence"?, "valid_until"? (RFC 3339), "inferred"?}`. The source
/// is `user:api` and the trust is the user's own, like the CLI's add.
pub fn add(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let subject = super::req_str(&body, "subject")?;
    let text = super::req_str(&body, "text")?;
    let tags = super::opt_str_list(&body, "tags");
    let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
    let mut claim = NewClaim::stated(&subject, &text, "user:api").with_tags(&tag_refs);
    if let Some(c) = body.get("confidence").and_then(|v| v.as_f64()) {
        if !(0.0..=1.0).contains(&c) {
            return Err(ApiError::bad_request("confidence 必须在 0..=1 之间"));
        }
        claim = claim.with_trust(hatch_core::Trust::User, c);
    }
    if let Some(v) = opt_str(&body, "valid_until") {
        claim.valid_until =
            Some(parse_rfc3339(&v).map_err(|e| {
                ApiError::bad_request(format!("valid_until 不是 RFC 3339 时间：{e}"))
            })?);
    }
    if opt_bool(&body, "inferred").unwrap_or(false) {
        claim = claim.inferred();
    }
    let now = hatch_core::time::now();
    let outcome = MemoryStore::new(&ctx.app.db).add(&claim, &now)?;
    let (id, duplicate) = match &outcome {
        hatch_memory::AddOutcome::Added(id) => (id.clone(), false),
        hatch_memory::AddOutcome::Duplicate(id) => (id.clone(), true),
    };
    Ok((201, json!({"id": id, "duplicate": duplicate})))
}

/// `GET /v1/memory/claims/{id}` — the claim plus its replacement
/// chain (what it superseded, what superseded it).
pub fn explain(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let ex = MemoryStore::new(&ctx.app.db)
        .explain(id)
        .map_err(|e| match e {
            hatch_core::Error::NotFound(_) => ApiError::not_found("记忆"),
            e => ApiError::from(e),
        })?;
    Ok((200, serde_json::to_value(&ex).unwrap_or(Value::Null)))
}

/// `GET /v1/memory/people` — the relationship index.
pub fn people(ctx: &ApiCtx) -> Result<(u16, Value), ApiError> {
    let list = MemoryStore::new(&ctx.app.db).people().list()?;
    Ok((
        200,
        json!({"people": serde_json::to_value(&list).unwrap_or(Value::Null)}),
    ))
}

/// `GET /v1/memory/people/{email}` — one person's page.
pub fn person(ctx: &ApiCtx, email: &str) -> Result<(u16, Value), ApiError> {
    let p = MemoryStore::new(&ctx.app.db).people().get(email)?;
    match p {
        Some(p) => Ok((200, serde_json::to_value(&p).unwrap_or(Value::Null))),
        None => Err(ApiError::not_found("联系人")),
    }
}
