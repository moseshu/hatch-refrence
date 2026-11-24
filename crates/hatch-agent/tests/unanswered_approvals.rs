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

const MARKER: &str = "Do not report these to the user as done";

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
            // Turn-end completeness check intercepts the first finish.
            ChatResponse::text("好的。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), AgentConfig::default(), "给老板发周报");
    // Completion is held until the proposed effect has executed and been checked.
    assert_eq!(r.stop, hatch_agent::StopReason::Answered);
    assert!(r.answer.as_deref().unwrap().contains("等待审批"));
    assert!(!r.answer.as_deref().unwrap().contains(MARKER));
    // The tool result already said "not yet executed".
    assert!(h.tool_result(&s, "1").contains("尚未执行"), "{}", h.tool_result(&s, "1"));
    // The approval is still pending at turn end.
    assert_eq!(r.approvals.len(), 1);
    let a = ApprovalQueue::new(&h.db).get(&r.approvals[0]).unwrap();
    assert_eq!(a.status, ApprovalStatus::Pending);
    assert!(!a.turn_id.is_empty(), "dispatch must persist the proposing turn");
    // The annotation is the last transcript row: the next round reads it
    // from history. It names the tool and the action.
    let rows = SessionStore::new(&h.db).transcript(&s).unwrap();
    let last = rows.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert!(last.content.contains("send_email"), "{}", last.content);
    assert!(last.content.contains("发送邮件：周报"), "{}", last.content);
    assert!(last.content.contains("still unapproved"), "{}", last.content);
    assert!(last.content.contains("not executed"), "{}", last.content);
    assert!(last.content.contains(MARKER), "{}", last.content);
    assert_eq!(annotation_rows(&h, &s).len(), 1);
}

#[test]
fn old_approval_does_not_replace_unrelated_answers_or_its_status_lookup() {
    let h = Harness::new();
    let submit = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![email_call("submit")]),
            ChatResponse::text("已提交审批。"),
            ChatResponse::text("已提交审批。"),
        ],
    ));
    let (first, session) = h.run(
        Router::single(submit),
        AgentConfig::default(),
        "给老板发周报",
    );
    let id = &first.approvals[0];
    let queue = ApprovalQueue::new(&h.db);
    assert_eq!(queue.get(id).unwrap().status, ApprovalStatus::Pending);

    let status_answer = format!("周报仍在等待审批，编号 {id}。");
    let status = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("status", "approval_status", json!({"id": id}))]),
            ChatResponse::text(&status_answer),
        ],
    ));
    let followup = run_on(
        &h,
        Router::single(status),
        AgentConfig::default(),
        &session,
        "刚才那封周报发了吗？",
    );
    assert!(h.tool_result(&session, "status").contains("status pending"));
    assert_eq!(followup.answer.as_deref(), Some(status_answer.as_str()));

    for expected_status in [ApprovalStatus::Pending, ApprovalStatus::Approved] {
        if expected_status == ApprovalStatus::Approved {
            queue
                .decide(id, true, "test", &hatch_core::time::now())
                .unwrap();
        }
        let model = Arc::new(ScriptedModel::ok(
            "m",
            vec![
                ChatResponse::calls(vec![call(
                    "lookup",
                    "memory_search",
                    json!({"query": "项目预算"}),
                )]),
                ChatResponse::text("未找到项目预算的记录。"),
            ],
        ));
        let unrelated = run_on(
            &h,
            Router::single(model),
            AgentConfig::default(),
            &session,
            "查一下项目预算的记录",
        );
        assert_eq!(unrelated.answer.as_deref(), Some("未找到项目预算的记录。"));
        assert!(unrelated.approvals.is_empty());
        assert_eq!(queue.get(id).unwrap().status, expected_status);
    }
}

#[test]
fn resumed_turn_still_reports_its_durable_pending_approval() {
    use hatch_agent::config::TurnContinuation;
    use hatch_agent::RunOptions;

    let h = Harness::new();
    let config = AgentConfig::default();
    let prompt = "给老板发周报";
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![email_call("submit")]),
            ChatResponse::text("已提交审批。"),
            ChatResponse::text("已提交审批。"),
        ],
    ));
    let (first, session) = h.run(Router::single(model), config, prompt);
    let approval = ApprovalQueue::new(&h.db).get(&first.approvals[0]).unwrap();
    let continuation = TurnContinuation {
        turn_id: approval.turn_id,
        next_step: 1,
        started_at: approval.created_at,
    };
    let router = Router::single(Arc::new(ScriptedModel::ok(
        "m",
        vec![ChatResponse::text("周报已经发送。")],
    )));
    let agent = Agent {
        db: &h.db,
        router: &router,
        tools: &h.tools,
        executors: &h.exec,
        gate: &h.gate,
        config: AgentConfig::default(),
    };
    let resumed = agent
        .run_with_continuation(
            &session,
            prompt,
            &hatch_core::time::now(),
            RunOptions::default(),
            Some(&continuation),
        )
        .unwrap();
    assert!(
        resumed.approvals.is_empty(),
        "the resumed report did not see a tool call"
    );
    assert!(resumed.answer.as_deref().unwrap().contains("等待审批"));
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
