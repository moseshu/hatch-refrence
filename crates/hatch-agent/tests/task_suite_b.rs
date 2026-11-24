//! Task suite, categories 3-4 (spec: task-suite-spec-2026-11-23.md):
//! intent boundaries & clarification, policy / authorization /
//! approval routing. Acceptance reads resource state.

#[path = "common/suite.rs"]
mod suite;

use hatch_agent::AgentConfig;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router, ToolCall};
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, Autonomy, PolicyConfig};
use serde_json::json;
use std::sync::Arc;
use suite::Suite;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn answer(r: &hatch_agent::TurnReport) -> &str {
    r.answer.as_deref().unwrap_or("")
}

// ---------------- category 3: intent boundaries ----------------

#[test]
fn intent_draft_only_never_sends() {
    let s = Suite::new(); // default policy: send needs approval
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "propose_email",
                json!({"to": ["boss@corp.example"], "subject": "周报", "body": "本周进展。"}),
            )]),
            ChatResponse::text("草稿已备好，待你确认后再发。"),
            ChatResponse::text("草稿已备好，待你确认后再发。"),
        ],
    ));
    s.run(
        &Router::single(model),
        AgentConfig::default(),
        "把周报写成草稿给 boss，先不要真发",
    );
    // Draft state: queued for the user, zero sends.
    assert_eq!(s.sent_count(), 0);
    let approvals = ApprovalQueue::new(&s.db).list(None).unwrap();
    let [draft] = approvals.as_slice() else {
        panic!("expected exactly one stored draft, got {approvals:?}");
    };
    assert_eq!(draft.status, ApprovalStatus::Pending);
    assert_eq!(draft.kind, "send_email");
    assert_eq!(draft.payload["to"], json!(["boss@corp.example"]));
    assert_eq!(draft.payload["subject"], "周报");
    assert_eq!(draft.payload["body"], "本周进展。");
}

#[test]
fn intent_correction_wins_over_first_value() {
    let mut policy = PolicyConfig::default();
    policy.set(ActionClass::WriteCalendar, Autonomy::ActNotify);
    let s = Suite::with_policy(policy);
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            // Turn 1: the model asks instead of guessing.
            ChatResponse::text("评审会定在下午三点还是五点？"),
            // Turn 2 (after the correction): creates at 17:00.
            ChatResponse::calls(vec![call(
                "1",
                "propose_event",
                json!({"title": "评审会", "start": "2026-11-24 17:00", "end": "2026-11-24 18:00"}),
            )]),
            ChatResponse::text("已按五点建好。"),
            ChatResponse::text("已按五点建好。"),
        ],
    ));
    let router = Router::single(model);
    let (r1, sess) = s.run(&router, AgentConfig::default(), "明天下午建个评审会");
    assert!(answer(&r1).contains("三点还是五点"), "{}", answer(&r1));
    assert_eq!(s.events().len(), 0);
    let _r2 = s.run_in(&router, AgentConfig::default(), &sess, "改到五点");
    let events = s.events();
    assert_eq!(events.len(), 1, "{events:?}");
    let start = events[0].starts_at.to_string();
    assert!(start.contains("17:00") || start.contains("09:00"), "{start}"); // 17:00+08 == 09:00Z
}

#[test]
fn intent_missing_referent_asks_instead_of_dispatching() {
    let s = Suite::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![ChatResponse::text("你说的“刚才那封”我这边没有上下文，是发给谁、什么主题？")],
    ));
    let (r, _sess) = s.run(&Router::single(model), AgentConfig::default(), "就发刚才那封");
    assert!(r.tool_calls.is_empty(), "{:?}", r.tool_calls);
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.approvals_with_status("pending"), 0);
    assert!(answer(&r).contains("谁"), "{}", answer(&r));
}

// ---------------- category 4: policy & approval routing ----------------

#[test]
fn policy_grant_executes_without_queueing() {
    // Sends are Irreversible: the class floor queues them even at
    // ActNotify. A standing grant is the product's auto-execute
    // route for sends (same mechanism the live bench relied on).
    let s = Suite::new();
    s.seed_person("boss@corp.example", "王总"); // known contact: no new-recipient escalation
    s.seed_grant(ActionClass::SendMessage, &["boss@corp.example"]);
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "propose_email",
                json!({"to": ["boss@corp.example"], "subject": "周报", "body": "本周进展。"}),
            )]),
            ChatResponse::text("已发送。"),
            ChatResponse::text("已发送。"),
        ],
    ));
    let (_r, _sess) = s.run(&Router::single(model), AgentConfig::default(), "把周报发给 boss@corp.example");
    assert_eq!(s.sent_count(), 1);
    assert_eq!(s.approvals_with_status("pending"), 0);
    assert_eq!(s.approvals_with_status("executed"), 1);
}

#[test]
fn policy_default_queues_with_correct_target_and_no_side_effect() {
    let s = Suite::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "propose_email",
                json!({"to": ["boss@corp.example"], "subject": "周报", "body": "本周进展。"}),
            )]),
            ChatResponse::text("已提交审批，待你确认。"),
            ChatResponse::text("已提交审批，待你确认。"),
        ],
    ));
    let (_r, _sess) = s.run(&Router::single(model), AgentConfig::default(), "把周报发给 boss@corp.example");
    // Before approval: zero side effects, exactly one queued item,
    // and the queued payload names the right target.
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.approvals_with_status("pending"), 1);
    let payload: String = s
        .db
        .conn()
        .query_row("SELECT payload FROM approvals WHERE status = 'pending'", [], |r| r.get(0))
        .unwrap();
    assert!(payload.contains("boss@corp.example"), "{payload}");
    assert!(payload.contains("周报"), "{payload}");
}
