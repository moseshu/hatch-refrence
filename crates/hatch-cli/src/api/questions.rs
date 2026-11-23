//! Question endpoints: the answer side of the `ask_user` tool.
//!
//! A pending question is an `ask_user` approval; these endpoints are
//! a typed view over that queue row. Answering records the choice in
//! the approval's note, which is what the suspended tool call reads
//! when it resumes the turn. Listing only shows questions still
//! waiting for an answer.

use super::{json_body, ApiCtx, ApiError};
use hatch_core::time::now;
use hatch_mcp::http_proto::HttpRequest;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_tools::builtin::ask_user::{answer_question, AskUserPayload, Choice, ASK_KIND};
use serde_json::{json, Value};

fn question_json(a: &hatch_policy::Approval) -> Value {
    let payload: Option<AskUserPayload> = serde_json::from_value(a.payload.clone()).ok();
    json!({
        "id": a.id,
        "question": payload.as_ref().map(|p| p.question.clone()),
        "options": payload.as_ref().map(|p| serde_json::to_value(&p.options).unwrap_or(Value::Null)),
        "allow_other": payload.as_ref().map(|p| p.allow_other),
        "origin": a.origin,
        "status": a.status.as_str(),
        "created_at": hatch_core::time::to_rfc3339(&a.created_at),
        "expires_at": hatch_core::time::to_rfc3339(&a.expires_at),
    })
}

/// `GET /v1/questions` — questions still waiting for an answer.
pub fn list(ctx: &ApiCtx, _req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let q = ApprovalQueue::new(&ctx.app.db);
    q.expire_due(&now())?;
    let items: Vec<Value> = q
        .list(Some(ApprovalStatus::Pending))?
        .iter()
        .filter(|a| a.kind == ASK_KIND)
        .map(question_json)
        .collect();
    Ok((200, json!({"questions": items})))
}

/// `POST /v1/questions/{id}/answer` — body
/// `{"choice"?: <0-based index | label>, "text"?: string}`. A
/// free-text-only answer (no choice) is accepted when the question
/// allows it. The waiting turn resumes with the recorded answer.
pub fn answer(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let choice = match body.get("choice") {
        Some(Value::Number(n)) => Some(Choice::Index(
            n.as_u64()
                .ok_or_else(|| ApiError::bad_request("choice 必须是非负整数下标或选项文字"))?
                as usize,
        )),
        Some(Value::String(s)) => Some(Choice::Label(s.clone())),
        Some(_) => return Err(ApiError::bad_request("choice 必须是非负整数下标或选项文字")),
        None => None,
    };
    let text = body.get("text").and_then(Value::as_str).map(str::to_string);
    let answer = answer_question(&ctx.app.db, id, choice, text, &now())?;
    Ok((
        200,
        json!({"id": id, "status": "answered", "answer": answer}),
    ))
}
