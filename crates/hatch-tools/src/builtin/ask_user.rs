//! `ask_user`: a structured question to the user, with 2–4 options.
//!
//! There is no pre-existing "block the turn until the user answers"
//! mechanism in hatch — approvals are deliberately asynchronous
//! events. This tool reuses the closest existing machinery instead of
//! inventing a parallel one: the question is a row in the same
//! persisted [`ApprovalQueue`] (kind `ask_user`, TTL = the timeout),
//! and the tool call polls it until the user answers through the
//! dedicated answer entry points (`POST /v1/questions/{id}/answer`,
//! `hatch questions answer`), the question is rejected, or the TTL
//! expires. The answer travels in the approval's `note` as JSON, so
//! the generic approve path also works: an `ask_user` executor is
//! registered whose only job is to echo the note back as its receipt,
//! which keeps the queue's executed-state bookkeeping harmless.
//!
//! Once the waiting call has read an approved answer it settles the
//! row to Executed itself (receipt = the answer note). An answered
//! question has no separate side effect left to execute, so leaving
//! the row in Approved would be wrong twice over: it would show as
//! still awaiting execution in the approval list, and the turn-end
//! verification counts Approved rows as pending and would replace the
//! turn's answer with a "still waiting" message.
//!
//! The tool is a generic interaction primitive: it carries a question
//! and options, nothing about any domain. While the call blocks, the
//! turn is suspended; the cancel token (Ctrl-C / turn cancel) always
//! wins, and the timeout guarantees the call never hangs forever.

use crate::tool::{arg_opt_str, arg_str, PlannedAction, Tool, ToolContext, ToolOutput};
use chrono::Duration;
use hatch_core::time::{now as now_ts, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, NewApproval, SubmitOutcome};
use hatch_store::Db;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Instant;

/// Approval kind used for questions.
pub const ASK_KIND: &str = "ask_user";

const DEFAULT_TIMEOUT_SECS: u64 = 300;
const MAX_TIMEOUT_SECS: u64 = 1800;
const POLL_MS: u64 = 200;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// Payload stored on the `ask_user` approval: exactly what the user
/// was asked, so answer entry points can validate a choice against it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AskUserPayload {
    pub question: String,
    pub options: Vec<AskOption>,
    #[serde(default = "default_true")]
    pub allow_other: bool,
}

fn default_true() -> bool {
    true
}

impl AskUserPayload {
    pub fn preview(&self) -> String {
        let mut s = format!("问题: {}", self.question);
        for (i, o) in self.options.iter().enumerate() {
            s.push_str(&format!("\n{}. {}", i + 1, o.label));
            if !o.description.is_empty() {
                s.push_str(&format!(" — {}", o.description));
            }
        }
        if self.allow_other {
            s.push_str("\n也可以自由输入其他回答。");
        }
        s
    }
}

fn parse_args(args: &Value) -> Result<(AskUserPayload, u64)> {
    let question = arg_str(args, "question")?.trim().to_string();
    if question.is_empty() {
        return Err(Error::Invalid("`question` must not be empty".into()));
    }
    let raw = args
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Invalid("`options` must be an array of 2-4 options".into()))?;
    if !(2..=4).contains(&raw.len()) {
        return Err(Error::Invalid(format!(
            "`options` must have 2-4 entries, got {}",
            raw.len()
        )));
    }
    let mut options = Vec::new();
    for o in raw {
        let opt = match o {
            Value::String(label) => AskOption {
                label: label.trim().to_string(),
                description: String::new(),
            },
            Value::Object(_) => AskOption {
                label: arg_str(o, "label")?.trim().to_string(),
                description: arg_opt_str(o, "description")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            },
            _ => {
                return Err(Error::Invalid(
                    "each option must be a string or {label, description}".into(),
                ))
            }
        };
        if opt.label.is_empty() {
            return Err(Error::Invalid("option labels must not be empty".into()));
        }
        if options.iter().any(|p: &AskOption| p.label == opt.label) {
            return Err(Error::Invalid(format!(
                "duplicate option label '{}'",
                opt.label
            )));
        }
        options.push(opt);
    }
    let allow_other = args
        .get("allow_other")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let timeout = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);
    Ok((
        AskUserPayload {
            question,
            options,
            allow_other,
        },
        timeout,
    ))
}

/// How the user picked an option: by position or by exact label.
pub enum Choice {
    Index(usize),
    Label(String),
}

/// Record the user's answer to a pending `ask_user` question. Shared
/// by the HTTP and CLI answer entry points. Returns the answer JSON
/// (`{choice, label, text}`) that the waiting tool call will read.
pub fn answer_question(
    db: &Db,
    id: &str,
    choice: Option<Choice>,
    text: Option<String>,
    now: &Timestamp,
) -> Result<Value> {
    let q = ApprovalQueue::new(db);
    let a = q.get(id)?;
    if a.kind != ASK_KIND {
        return Err(Error::Invalid(format!(
            "{id} is not a question (kind {})",
            a.kind
        )));
    }
    if a.status != ApprovalStatus::Pending {
        return Err(Error::Invalid(format!(
            "question {id} is already {}",
            a.status.as_str()
        )));
    }
    let payload: AskUserPayload = serde_json::from_value(a.payload.clone())
        .map_err(|e| Error::Invalid(format!("bad ask_user payload: {e}")))?;
    let text = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let (idx, label) = match choice {
        Some(Choice::Index(i)) => {
            let o = payload.options.get(i).ok_or_else(|| {
                Error::Invalid(format!(
                    "choice {} out of range ({} options)",
                    i + 1,
                    payload.options.len()
                ))
            })?;
            (Some(i), o.label.clone())
        }
        Some(Choice::Label(l)) => {
            let (i, o) = payload
                .options
                .iter()
                .enumerate()
                .find(|(_, o)| o.label == l)
                .ok_or_else(|| Error::Invalid(format!("no option labelled '{l}'")))?;
            (Some(i), o.label.clone())
        }
        None => (None, "其他".to_string()),
    };
    if idx.is_none() && text.is_none() {
        return Err(Error::Invalid("answer needs a choice or free text".into()));
    }
    if text.is_some() && idx.is_none() && !payload.allow_other {
        return Err(Error::Invalid(
            "this question does not allow free-text answers".into(),
        ));
    }
    let answer = json!({"choice": idx, "label": label, "text": text});
    let status = q.decide(id, true, &answer.to_string(), now)?;
    if status != ApprovalStatus::Approved {
        return Err(Error::Invalid(format!(
            "question {id} is {} (expired questions cannot be answered)",
            status.as_str()
        )));
    }
    Ok(answer)
}

/// Format one recorded answer as the clarification text: the chosen
/// label, plus free text when the user added any.
fn clarification_answer(note: &Value) -> Option<String> {
    if note.is_object() {
        let label = note.get("label").and_then(Value::as_str).unwrap_or("").trim();
        let text = note.get("text").and_then(Value::as_str).unwrap_or("").trim();
        match (label.is_empty(), text.is_empty()) {
            (true, true) => None,
            (false, true) => Some(label.to_string()),
            (true, false) => Some(text.to_string()),
            (false, false) => Some(format!("{label}（{text}）")),
        }
    } else if let Some(text) = note.as_str() {
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    } else {
        None
    }
}

/// Answers the user gave to `ask_user` questions of one turn, as
/// trusted intent clarifications. Both answer entry points (`POST
/// /v1/questions/{id}/answer` and `hatch questions answer`) record
/// through [`answer_question`], which decides the approval row this
/// reads: the question id and text come from the stored payload, the
/// answer from its note. The binding is the turn that asked: the
/// row's `turn_id` (written when the question was submitted) must
/// equal this turn's id. Origin and `decided_at >= since` stay as
/// auxiliary constraints, but they are not the binding — two turns
/// of one session share an origin, and whole-second timestamps let
/// a previous turn's answer fall into a new turn that starts in the
/// same second. A row recorded without a turn id (empty) never
/// matches a real turn, so a legacy unbound answer is unusable
/// rather than borrowed.
pub fn answered_clarifications(
    db: &Db,
    origin: &str,
    turn_id: &str,
    since: &Timestamp,
) -> Result<Vec<crate::intent::IntentClarification>> {
    let mut found: Vec<(Timestamp, crate::intent::IntentClarification)> = Vec::new();
    // Stored timestamps keep only whole seconds, so compare in
    // whole seconds: an answer given in the same second the turn
    // started still counts (its question was asked in this turn).
    let since_secs = since.timestamp();
    for a in ApprovalQueue::new(db).list(Some(ApprovalStatus::Executed))? {
        if a.kind != ASK_KIND || a.origin != origin || a.turn_id != turn_id {
            continue;
        }
        let Some(decided) = a.decided_at.filter(|d| d.timestamp() >= since_secs) else {
            continue;
        };
        let Ok(payload) = serde_json::from_value::<AskUserPayload>(a.payload.clone()) else {
            continue;
        };
        let Some(answer) = serde_json::from_str::<Value>(&a.note)
            .ok()
            .and_then(|n| clarification_answer(&n))
            .or_else(|| clarification_answer(&Value::String(a.note.clone())))
        else {
            continue;
        };
        found.push((
            decided,
            crate::intent::IntentClarification {
                question_id: a.id,
                question: payload.question,
                answer,
            },
        ));
    }
    found.sort_by_key(|(d, _)| *d);
    Ok(found.into_iter().map(|(_, c)| c).collect())
}

/// Turn a decided approval's note into the tool result the model sees.
fn result_from_note(note: &str) -> Value {
    let note = note.trim();
    if note.is_empty() {
        return json!({"answered": true, "choice": null, "label": null, "text": null,
                      "note": "用户已确认，但未给出具体选项"});
    }
    match serde_json::from_str::<Value>(note) {
        Ok(v) if v.is_object() => {
            let mut out = json!({"answered": true});
            if let (Value::Object(dst), Value::Object(src)) = (&mut out, &v) {
                for k in ["choice", "label", "text"] {
                    dst.insert(k.into(), src.get(k).cloned().unwrap_or(Value::Null));
                }
            }
            out
        }
        // Generic approve with a free-form note: treat it as the answer text.
        _ => json!({"answered": true, "choice": null, "label": null, "text": note}),
    }
}

pub struct AskUser;

/// Idempotency key for a question. The turn that asks it is part
/// of the key: the same question asked again in a later turn —
/// same origin, same model call id (Gemini reuses `gemini_call_0`
/// across steps and turns) — creates an independent row instead of
/// returning the old decided row with its old answer. The question
/// payload is part of the key too: the same question text with
/// different options (or a different `allow_other`) is a different
/// question — within one turn a provider that synthesises call ids
/// can reuse the same id across steps, so the text alone cannot
/// tell a re-ask with new options from a resubmission. The payload
/// serializes canonically (struct field order, options in order),
/// so option order is significant. Within one turn the key is
/// unchanged for identical arguments, so a resumed dispatch still
/// finds the same row: the resume path rebuilds the original turn
/// id from the recorded unit.
fn question_key(origin: &str, call_id: &str, turn_id: &str, payload: &AskUserPayload) -> String {
    // serde_json writes struct fields in declaration order, so the
    // same question always serializes the same way; to_string cannot
    // fail for this payload, and the fallback keeps the question
    // text in the key rather than dropping the payload entirely.
    let canonical =
        serde_json::to_string(payload).unwrap_or_else(|_| payload.question.clone());
    ids::stable_id("ask", &[origin, call_id, turn_id, &canonical])
}

impl Tool for AskUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a structured question with 2-4 options and WAIT for their choice — this call blocks the turn until the user answers (or the timeout expires). Use it when a decision genuinely belongs to the user and you cannot continue without it (which of several approaches, confirm a preference); do not use it for things you can decide or look up yourself. Do NOT use show_options for this: show_options only displays options as a card and does not wait for an answer. The result is JSON: {answered, choice, label, text}. answered=false means the user declined or did not answer in time — proceed with your best judgement and say so.".into(),
            parameters: json!({"type": "object", "properties": {
                "question": {"type": "string", "description": "the question, in the user's language"},
                "options": {"type": "array", "minItems": 2, "maxItems": 4, "items": {"type": "object", "properties": {
                    "label": {"type": "string"},
                    "description": {"type": "string"}
                }, "required": ["label"]}, "description": "2-4 options (plain strings also accepted)"},
                "allow_other": {"type": "boolean", "description": "allow a free-text answer instead of an option (default true)"},
                "timeout_secs": {"type": "integer", "description": "how long to wait for an answer (default 300, max 1800)"}
            }, "required": ["question", "options"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        let (p, _) = parse_args(args)?;
        Ok(PlannedAction {
            summary: format!("向用户提问：{}", p.question),
            ..Default::default()
        })
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let (payload, timeout_secs) = parse_args(args)?;
        let q = ApprovalQueue::new(ctx.db);
        let key = question_key(&ctx.origin, &ctx.call_id, &ctx.turn_id, &payload);
        let existing = match q.by_key(&key)? {
            Some(existing) => Some(existing),
            None if !ctx.turn_id.is_empty() => {
                // Before v28 the key omitted the options. Only reuse an
                // old record if it asked exactly the same question.
                let legacy_key = ids::stable_id(
                    "ask",
                    &[&ctx.origin, &ctx.call_id, &payload.question, &ctx.turn_id],
                );
                q.by_key(&legacy_key)?.filter(|old| {
                    old.kind == ASK_KIND
                        && old.origin == ctx.origin
                        && old.turn_id == ctx.turn_id
                        && serde_json::from_value::<AskUserPayload>(old.payload.clone())
                            .is_ok_and(|saved| saved == payload)
                })
            }
            None => None,
        };
        let id = if let Some(existing) = existing {
            existing.id
        } else {
            match q.submit(
                &NewApproval {
                    turn_id: ctx.turn_id.clone(),
                    idempotency_key: key,
                    kind: ASK_KIND.into(),
                    class: ActionClass::ReadPersonal,
                    title: format!("问题：{}", payload.question),
                    preview: payload.preview(),
                    payload: serde_json::to_value(&payload)?,
                    reasons: vec![],
                    origin: ctx.origin.clone(),
                    ttl: Duration::seconds(timeout_secs as i64),
                },
                &ctx.now,
            )? {
                SubmitOutcome::Created(id) | SubmitOutcome::Existing(id, _) => id,
            }
        };
        let deadline = Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            let a = q.get(&id)?;
            match a.status {
                ApprovalStatus::Approved => {
                    // The answer is in hand; settle the row so it is
                    // not mistaken for work still awaiting execution.
                    // Best-effort: a settle failure must not lose an
                    // answer the user already gave.
                    let note = a.note.clone();
                    let _ = q.mark_executed(&id, &note, &ctx.now);
                    return Ok(ToolOutput::system(result_from_note(&note).to_string()));
                }
                ApprovalStatus::Executed => {
                    return Ok(ToolOutput::system(result_from_note(&a.note).to_string()));
                }
                ApprovalStatus::Rejected => {
                    return Ok(ToolOutput::system(
                        json!({"answered": false, "reason": "user_declined"}).to_string(),
                    ));
                }
                ApprovalStatus::Expired | ApprovalStatus::Invalidated => {
                    return Ok(ToolOutput::system(
                        json!({"answered": false, "reason": "timeout", "timeout_secs": timeout_secs})
                            .to_string(),
                    ));
                }
                ApprovalStatus::Failed | ApprovalStatus::NeedsReview => {
                    return Ok(ToolOutput::system(
                        json!({"answered": false, "reason": "question_failed"}).to_string(),
                    ));
                }
                ApprovalStatus::Pending => {}
            }
            if Instant::now() >= deadline {
                // Mirror approval expiry: flip it so no late answer can
                // be recorded against a question nobody is waiting on.
                let _ = q.expire_due(&now_ts());
                return Ok(ToolOutput::system(
                    json!({"answered": false, "reason": "timeout", "timeout_secs": timeout_secs})
                        .to_string(),
                ));
            }
            if ctx.cancel.sleep(std::time::Duration::from_millis(POLL_MS)) {
                return Err(Error::Cancelled(
                    ctx.cancel.reason().unwrap_or_else(|| "cancelled".into()),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_store::Db;

    fn ctx<'a>(db: &'a Db) -> ToolContext<'a> {
        ctx_for_turn(db, "turn-1")
    }

    fn ctx_for_turn<'a>(db: &'a Db, turn_id: &str) -> ToolContext<'a> {
        ToolContext {
            turn_id: turn_id.into(),
            db,
            now: now_ts(),
            tz: "Asia/Shanghai".parse().unwrap(),
            user_email: "me@example.com".into(),
            user_name: "Tester".into(),
            origin: "test".into(),
            tainted: false,
            call_id: "c1".into(),
            cancel: hatch_core::CancelToken::new(),
            intent_review: None,
        }
    }

    fn submit_question(db: &Db) -> String {
        let ctx = ctx(db);
        let q = ApprovalQueue::new(db);
        // The payload the key is derived from: build it once so the
        // key and the stored payload can never drift apart.
        let payload = AskUserPayload {
            question: "选哪个".into(),
            options: vec![
                // Plain labels, no descriptions: exactly the payload
                // AskUser::call derives from the ["甲", "乙"] args
                // the tests below pass it, so the key matches.
                AskOption {
                    label: "甲".into(),
                    description: String::new(),
                },
                AskOption {
                    label: "乙".into(),
                    description: String::new(),
                },
            ],
            allow_other: true,
        };
        match q
            .submit(
                &NewApproval {
                    // Same key AskUser::call derives for ctx() above,
                    // so the decided-row test reuses this question.
                    // Built through the same helper, so the two can
                    // never drift apart again.
                    idempotency_key: question_key("test", "c1", "turn-1", &payload),
                    kind: ASK_KIND.into(),
                    class: ActionClass::ReadPersonal,
                    title: "问题：选哪个".into(),
                    preview: "问题: 选哪个".into(),
                    payload: serde_json::to_value(&payload).unwrap(),
                    reasons: vec![],
                    origin: "test".into(),
                    turn_id: "turn-1".into(),
                    ttl: Duration::minutes(5),
                },
                &ctx.now,
            )
            .unwrap()
        {
            SubmitOutcome::Created(id) => id,
            SubmitOutcome::Existing(id, _) => id,
        }
    }

    #[test]
    fn args_require_two_to_four_unique_options() {
        let db = Db::open_in_memory().unwrap();
        let ctx = ctx(&db);
        let tool = AskUser;
        assert!(tool
            .plan(&json!({"question": "q", "options": ["a"]}), &ctx)
            .is_err());
        assert!(tool
            .plan(
                &json!({"question": "q", "options": ["a", "b", "c", "d", "e"]}),
                &ctx
            )
            .is_err());
        assert!(tool
            .plan(&json!({"question": "q", "options": ["a", "a"]}), &ctx)
            .is_err());
        assert!(tool
            .plan(&json!({"question": "  ", "options": ["a", "b"]}), &ctx)
            .is_err());
        let (p, t) = parse_args(&json!({"question": "q", "options": [
            {"label": "a", "description": "d"}, "b"
        ]}))
        .unwrap();
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.options[0].description, "d");
        assert_eq!(t, 300);
    }

    #[test]
    fn answer_by_index_label_and_free_text() {
        let db = Db::open_in_memory().unwrap();
        let now = now_ts();

        let id = submit_question(&db);
        let ans = answer_question(&db, &id, Some(Choice::Index(1)), None, &now).unwrap();
        assert_eq!(ans["label"], "乙");
        // Second answer is refused.
        assert!(answer_question(&db, &id, Some(Choice::Index(0)), None, &now).is_err());

        let db = Db::open_in_memory().unwrap();
        let id = submit_question(&db);
        let ans = answer_question(&db, &id, Some(Choice::Label("甲".into())), None, &now).unwrap();
        assert_eq!(ans["choice"], 0);
        assert!(answer_question(&db, &id, Some(Choice::Label("丙".into())), None, &now).is_err());

        let db = Db::open_in_memory().unwrap();
        let id = submit_question(&db);
        let ans = answer_question(&db, &id, None, Some("都不选，我自己来".into()), &now).unwrap();
        assert_eq!(ans["text"], "都不选，我自己来");
        assert!(ans["choice"].is_null());
    }

    #[test]
    fn answered_clarifications_bind_question_id_and_turn() {
        let db = Db::open_in_memory().unwrap();
        let before = now_ts();
        let id = submit_question(&db);
        // Unanswered: nothing to clarify with.
        assert!(answered_clarifications(&db, "test", "turn-1", &before)
            .unwrap()
            .is_empty());
        let ans = answer_question(
            &db,
            &id,
            Some(Choice::Index(1)),
            Some("越快越好".into()),
            &now_ts(),
        )
        .unwrap();
        // The waiting tool call settles an answered question to
        // Executed when it reads the answer; that is the state the
        // clarification reader looks for.
        ApprovalQueue::new(&db)
            .mark_executed(&id, &ans.to_string(), &now_ts())
            .unwrap();
        let list = answered_clarifications(&db, "test", "turn-1", &before).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].question_id, id);
        assert_eq!(list[0].question, "选哪个");
        assert_eq!(list[0].answer, "乙（越快越好）");
        // A later turn (since well after the answer) does not
        // inherit it, and neither does another origin.
        let later = before + chrono::Duration::hours(1);
        assert!(answered_clarifications(&db, "test", "turn-1", &later)
            .unwrap()
            .is_empty());
        assert!(
            answered_clarifications(&db, "chat:other", "turn-1", &before)
                .unwrap()
                .is_empty()
        );
    }

    /// RED (review round 2, P1): clarifications are bound to the turn
    /// that asked the question. A new turn starting in the same
    /// second must not inherit the previous turn's answer — the old
    /// origin + whole-second `decided_at` filter leaks exactly there.
    #[test]
    fn answered_clarifications_are_bound_to_turn_id() {
        let db = Db::open_in_memory().unwrap();
        let before = now_ts();
        let id = submit_question(&db);
        let ans = answer_question(&db, &id, Some(Choice::Index(1)), None, &now_ts()).unwrap();
        ApprovalQueue::new(&db)
            .mark_executed(&id, &ans.to_string(), &now_ts())
            .unwrap();
        // Same second, different turn: the answer was decided after
        // `before` and under the same origin, so only the turn
        // binding can keep it out.
        let leaked = answered_clarifications(&db, "test", "turn-2", &before).unwrap();
        assert!(
            leaked.is_empty(),
            "previous turn's answer leaked into a new turn: {leaked:?}"
        );
        // The turn that asked still reads it.
        let list = answered_clarifications(&db, "test", "turn-1", &before).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].question_id, id);
        // Legacy rows recorded without a turn id never match a real
        // turn: an unbound answer is unusable, never borrowed.
        db.conn()
            .execute("UPDATE approvals SET turn_id = '' WHERE id = ?1", [&id])
            .unwrap();
        let list = answered_clarifications(&db, "test", "turn-1", &before).unwrap();
        assert!(list.is_empty(), "unbound legacy answer leaked: {list:?}");
    }

    #[test]
    fn legacy_answer_is_ignored_when_question_options_change() {
        let db = Db::open_in_memory().unwrap();
        let old_id = submit_question(&db);
        let legacy_key = ids::stable_id("ask", &["test", "c1", "选哪个", "turn-1"]);
        db.conn()
            .execute(
                "UPDATE approvals SET idempotency_key = ?1 WHERE id = ?2",
                hatch_store::params![legacy_key, old_id],
            )
            .unwrap();
        answer_question(&db, &old_id, Some(Choice::Index(0)), None, &now_ts()).unwrap();

        let out = AskUser
            .call(
                &json!({"question": "选哪个", "options": ["丙", "丁"], "timeout_secs": 1}),
                &ctx(&db),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&out.content).unwrap()["answered"],
            false
        );
        let queue = ApprovalQueue::new(&db);
        assert_eq!(queue.list(None).unwrap().len(), 2);
        assert_eq!(queue.get(&old_id).unwrap().status, ApprovalStatus::Approved);
    }

    #[test]
    fn timeout_returns_unanswered_and_expires_the_question() {
        let db = Db::open_in_memory().unwrap();
        let ctx = ctx(&db);
        let out = AskUser
            .call(
                &json!({"question": "选哪个", "options": ["甲", "乙"], "timeout_secs": 1}),
                &ctx,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["answered"], false);
        assert_eq!(v["reason"], "timeout");
        let rows = ApprovalQueue::new(&db).list(None).unwrap();
        assert_eq!(rows[0].status, ApprovalStatus::Expired);
    }

    #[test]
    fn answered_question_is_settled_to_executed() {
        let db = Db::open_in_memory().unwrap();
        let id = submit_question(&db);
        answer_question(&db, &id, Some(Choice::Index(1)), None, &now_ts()).unwrap();
        // A fresh call with the same idempotency key sees the decided row.
        let ctx = ctx(&db);
        let out = AskUser
            .call(&json!({"question": "选哪个", "options": ["甲", "乙"]}), &ctx)
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["answered"], true);
        assert_eq!(v["label"], "乙");
        // The answered row is settled, not left Approved: the turn-end
        // verification counts Approved rows as pending and would
        // replace the turn's answer with a "still waiting" message.
        let row = ApprovalQueue::new(&db).get(&id).unwrap();
        assert_eq!(row.status, ApprovalStatus::Executed);
    }

    #[test]
    fn description_distinguishes_waiting_from_show_options() {
        let d = AskUser.spec().description;
        assert!(d.contains("WAIT"), "{d}");
        assert!(d.contains("show_options"), "{d}");
        assert!(d.contains("does not wait"), "{d}");
    }

    #[test]
    fn rejected_question_returns_declined() {
        let db = Db::open_in_memory().unwrap();
        let id = submit_question(&db);
        ApprovalQueue::new(&db)
            .decide(&id, false, "不想答", &now_ts())
            .unwrap();
        // A fresh call with the same idempotency key sees the decided row.
        let ctx = ctx(&db);
        let out = AskUser
            .call(
                &json!({"question": "选哪个", "options": ["甲", "乙"]}),
                &ctx,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["answered"], false);
        assert_eq!(v["reason"], "user_declined");
    }

    /// RED (review round 3, P2): the same question asked again in
    /// a later turn — same origin, same model call id — must not
    /// return the previous turn's answer. The idempotency key has
    /// no turn in it, so submit hands back the old decided row and
    /// the tool returns its answer as this turn's result.
    #[test]
    fn same_question_next_turn_does_not_reuse_old_answer() {
        let db = Db::open_in_memory().unwrap();
        // Turn 1 asks and gets an answer.
        let id1 = submit_question(&db);
        let ans = answer_question(&db, &id1, Some(Choice::Index(1)), None, &now_ts()).unwrap();
        ApprovalQueue::new(&db)
            .mark_executed(&id1, &ans.to_string(), &now_ts())
            .unwrap();
        // Turn 2 asks the same question with the same model call
        // id. It must wait for its own answer (here: time out),
        // not take turn 1's answer as its result.
        let ctx2 = ctx_for_turn(&db, "turn-2");
        let out = AskUser
            .call(
                &json!({"question": "选哪个", "options": ["甲", "乙"], "timeout_secs": 1}),
                &ctx2,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["answered"], false, "turn 2 must not inherit turn 1's answer: {v}");
        assert_eq!(v["reason"], "timeout");
        // Turn 2 asked its own question: two rows, turn 1's
        // answered row untouched.
        let rows = ApprovalQueue::new(&db).list(None).unwrap();
        assert_eq!(rows.len(), 2, "a new turn asks a new question: {rows:?}");
        assert_eq!(rows.iter().filter(|r| r.turn_id == "turn-1").count(), 1);
        assert_eq!(rows.iter().filter(|r| r.turn_id == "turn-2").count(), 1);
    }

    /// Same turn, same call, same question: resubmission is still
    /// idempotent — the resumed dispatch finds the same row.
    #[test]
    fn same_turn_resubmission_stays_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let id1 = submit_question(&db);
        let id2 = submit_question(&db);
        assert_eq!(id1, id2, "same turn re-asks the same question row");
    }

    /// Submit a question with plain-label options through the same
    /// key helper `AskUser::call` uses, so the test cannot drift
    /// from the production key.
    fn submit_question_options(db: &Db, labels: &[&str]) -> String {
        let payload = AskUserPayload {
            question: "选哪个".into(),
            options: labels
                .iter()
                .map(|l| AskOption {
                    label: l.to_string(),
                    description: String::new(),
                })
                .collect(),
            allow_other: true,
        };
        let ctx = ctx(db);
        match ApprovalQueue::new(db)
            .submit(
                &NewApproval {
                    idempotency_key: question_key("test", "c1", "turn-1", &payload),
                    kind: ASK_KIND.into(),
                    class: ActionClass::ReadPersonal,
                    title: "问题：选哪个".into(),
                    preview: payload.preview(),
                    payload: serde_json::to_value(&payload).unwrap(),
                    reasons: vec![],
                    origin: "test".into(),
                    turn_id: "turn-1".into(),
                    ttl: Duration::minutes(5),
                },
                &ctx.now,
            )
            .unwrap()
        {
            SubmitOutcome::Created(id) => id,
            SubmitOutcome::Existing(id, _) => id,
        }
    }

    /// RED (review round 4, P2): same turn, same model call id,
    /// same question text, different options — the agent kept the
    /// question and changed the options. The key only covers the
    /// question text, so submit hands back the old decided row and
    /// the user gets the previous answer instead of the new options.
    #[test]
    fn same_question_text_different_options_asks_again() {
        let db = Db::open_in_memory().unwrap();
        // First ask, answered 乙.
        let id1 = submit_question_options(&db, &["甲", "乙"]);
        let ans = answer_question(&db, &id1, Some(Choice::Index(1)), None, &now_ts()).unwrap();
        assert_eq!(ans["label"], "乙");
        ApprovalQueue::new(&db)
            .mark_executed(&id1, &ans.to_string(), &now_ts())
            .unwrap();
        // Same turn, same call id, same question text — new options.
        let ctx = ctx(&db);
        let out = AskUser
            .call(
                &json!({"question": "选哪个", "options": ["甲", "丙"], "timeout_secs": 1}),
                &ctx,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(
            v["answered"], false,
            "the new options must wait for their own answer: {v}"
        );
        assert_eq!(v["reason"], "timeout");
        // A new question row, showing the new options.
        let rows = ApprovalQueue::new(&db).list(None).unwrap();
        assert_eq!(
            rows.len(),
            2,
            "changed options ask a new question: {rows:?}"
        );
        let second = rows.iter().find(|r| r.id != id1).unwrap();
        assert!(
            second.preview.contains("丙"),
            "new options shown: {}",
            second.preview
        );
        let payload: AskUserPayload = serde_json::from_value(second.payload.clone()).unwrap();
        assert_eq!(
            payload
                .options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>(),
            vec!["甲", "丙"]
        );
    }

    /// Option order is significant: reordering the options is a
    /// different question, not a resubmission of the same one.
    #[test]
    fn option_order_swapped_is_a_different_question() {
        let db = Db::open_in_memory().unwrap();
        let id1 = submit_question_options(&db, &["甲", "乙"]);
        let id2 = submit_question_options(&db, &["乙", "甲"]);
        assert_ne!(
            id1, id2,
            "swapped options must not share a question row"
        );
        let rows = ApprovalQueue::new(&db).list(None).unwrap();
        assert_eq!(rows.len(), 2);
    }

    /// The key separates turns, payloads, and nothing else: same
    /// turn and same payload reuses, a new turn or a changed payload
    /// never collides.
    #[test]
    fn question_key_differs_across_turns_and_payloads() {
        let payload = AskUserPayload {
            question: "选哪个".into(),
            options: vec![
                AskOption {
                    label: "甲".into(),
                    description: String::new(),
                },
                AskOption {
                    label: "乙".into(),
                    description: String::new(),
                },
            ],
            allow_other: true,
        };
        assert_eq!(
            question_key("test", "c1", "turn-1", &payload),
            question_key("test", "c1", "turn-1", &payload)
        );
        assert_ne!(
            question_key("test", "c1", "turn-1", &payload),
            question_key("test", "c1", "turn-2", &payload)
        );
        let changed = AskUserPayload {
            options: vec![
                AskOption {
                    label: "甲".into(),
                    description: String::new(),
                },
                AskOption {
                    label: "丙".into(),
                    description: String::new(),
                },
            ],
            ..payload.clone()
        };
        assert_ne!(
            question_key("test", "c1", "turn-1", &payload),
            question_key("test", "c1", "turn-1", &changed),
            "changed options are a different question"
        );
        let swapped = AskUserPayload {
            options: vec![
                AskOption {
                    label: "乙".into(),
                    description: String::new(),
                },
                AskOption {
                    label: "甲".into(),
                    description: String::new(),
                },
            ],
            ..payload.clone()
        };
        assert_ne!(
            question_key("test", "c1", "turn-1", &payload),
            question_key("test", "c1", "turn-1", &swapped),
            "option order is significant"
        );
    }
}
