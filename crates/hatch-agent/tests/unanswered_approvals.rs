//! Unanswered-approval annotation: an approval this turn raised that is
//! still pending at turn end is named in the session, so the next round
//! sees it and does not report it as done.

#[path = "common/mod.rs"]
mod common;

use common::{call, Harness};
use hatch_agent::{Agent, AgentConfig, SessionStore, TurnReport};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use serde_json::json;
use std::sync::Arc;

const MARKER: &str = "不要向用户汇报成已完成";

fn email_call(id: &str) -> hatch_llm::ToolCall {
    call(
        id,
        "propose_email",
        json!({"to": ["boss@corp.com"], "subject": "周报", "body": "见附件"}),
    )
}

fn annotation_rows(h: &Harness, session: &str) -> Vec<String> {
    SessionStore::new(&h.db)
        .transcript(session)
        .unwrap()
        .into_iter()
        .filter(|r| r.content.contains(MARKER))
        .map(|r| r.content)
        .collect()
}

fn run_on(h: &Harness, router: Router, config: AgentConfig, session: &str, input: &str) -> TurnReport {
    let agent = Agent {
        db: &h.db,
        router: &router,
        tools: &h.tools,
        executors: &h.exec,
        gate: &h.gate,
        config,
    };
    let now = hatch_core::time::now();
    agent.run_turn(session, input, &now).unwrap()
}

#[test]
fn unanswered_approval_is_annotated_for_the_next_round() {
    let h = Harness::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![email_call("1")]),
            ChatResponse::text("好的。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), AgentConfig::default(), "给老板发周报");
    // The user-visible answer is untouched by the annotation.
    assert_eq!(r.answer.as_deref(), Some("好的。"));
    assert!(!r.answer.as_deref().unwrap().contains(MARKER));
    // The tool result already said "not yet executed".
    assert!(h.tool_result(&s, "1").contains("尚未执行"), "{}", h.tool_result(&s, "1"));
    // The approval is still pending at turn end.
    assert_eq!(r.approvals.len(), 1);
    let a = ApprovalQueue::new(&h.db).get(&r.approvals[0]).unwrap();
    assert_eq!(a.status, ApprovalStatus::Pending);
    // The annotation is the last transcript row: the next round reads it
    // from history. It names the tool and the action.
    let rows = SessionStore::new(&h.db).transcript(&s).unwrap();
    let last = rows.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert!(last.content.contains("send_email"), "{}", last.content);
    assert!(last.content.contains("发送邮件：周报"), "{}", last.content);
    assert!(last.content.contains("未获批准"), "{}", last.content);
    assert!(last.content.contains("未执行"), "{}", last.content);
    assert!(last.content.contains(MARKER), "{}", last.content);
    assert_eq!(annotation_rows(&h, &s).len(), 1);
}

#[test]
fn no_annotation_without_unanswered_approvals() {
    let h = Harness::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "memory_search", json!({"query": "x"}))]),
            ChatResponse::text("没有。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), AgentConfig::default(), "查记忆");
    assert!(r.approvals.is_empty());
    assert!(annotation_rows(&h, &s).is_empty());
}

#[test]
fn max_steps_end_still_annotates() {
    let h = Harness::new();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::calls(vec![email_call("1")])]));
    let cfg = AgentConfig { max_steps: 1, ..Default::default() };
    let (r, s) = h.run(Router::single(model), cfg, "给老板发周报");
    assert_eq!(r.tool_calls, vec!["propose_email"]);
    assert_eq!(annotation_rows(&h, &s).len(), 1);
}

#[test]
fn decided_approval_is_not_annotated_again() {
    let h = Harness::new();
    let now = hatch_core::time::now();
    let s = SessionStore::new(&h.db).create("t", &now).unwrap();
    // Turn 1: the email queues and is annotated.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![ChatResponse::calls(vec![email_call("1")]), ChatResponse::text("好的。")],
    ));
    let r1 = run_on(&h, Router::single(model), AgentConfig::default(), &s, "给老板发周报");
    assert_eq!(annotation_rows(&h, &s).len(), 1);
    assert_eq!(r1.approvals.len(), 1);
    // The user approves it between turns (execution is separate; the
    // action is answered, so it must not be annotated again).
    ApprovalQueue::new(&h.db)
        .decide(&r1.approvals[0], true, "test", &now)
        .unwrap();
    // Turn 2: the same action hits the already-handled path.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![ChatResponse::calls(vec![email_call("1")]), ChatResponse::text("好的。")],
    ));
    let r2 = run_on(&h, Router::single(model), AgentConfig::default(), &s, "再发一次");
    // The second call hits the already-handled path and returns the same id.
    assert_eq!(r2.approvals, r1.approvals);
    assert_eq!(annotation_rows(&h, &s).len(), 1, "decided approval must not be annotated again");
}
