//! Task suite expansion, batch 2 (spec: task-suite-spec-2026-11-23.md):
//! categories 3-4 — intent boundaries (multi-turn confirmation,
//! amount conflict, ask_user round-trip) and policy routing
//! (browser find_tool discovery on a default turn, PR #116; L0
//! judge-unavailable in Enforce). Scripted models replay fixed
//! decisions; acceptance reads resource state, never the agent's
//! own report.
//!
//! Each test documents its four acceptance fields in the header
//! comment: (1) final resource state, (2) allowed side effects,
//! (3) state the answer must name, (4) step/call budget.

#[path = "common/suite.rs"]
mod suite;

use hatch_agent::bridge::{allowed_specs, ToolBridge};
use hatch_agent::{AgentConfig, StopReason};
use hatch_llm::scripted::{FnModel, ScriptedModel};
use hatch_llm::{ChatRequest, ChatResponse, LlmError, Router, ToolCall};
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus};
use hatch_tools::builtin::ask_user::{answer_question, Choice, ASK_KIND};
use hatch_tools::IntentGateMode;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
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

fn mail_to_boss() -> serde_json::Value {
    json!({"to": ["boss@corp.example"], "subject": "周报", "body": "本周进展。"})
}

// ---------------- category 3: intent boundaries ----------------

#[test]
fn intent_multiturn_confirm_resolvable() {
    // (1) turn 1 sends once; turn 2 ("就发刚才那封") resolves the
    //     referent to the same mail — and the idempotency ledger
    //     refuses to send the identical mail twice, so the outbox
    //     still holds exactly one send; (2) no other side effects;
    // (3) turn 2 says the mail already went out instead of
    //     claiming a re-send; (4) two short turns.
    let s = Suite::new();
    s.seed_person("boss@corp.example", "王总");
    s.seed_grant(ActionClass::SendMessage, &["boss@corp.example"]);
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            // Turn 1: the first send.
            ChatResponse::calls(vec![call("1", "propose_email", mail_to_boss())]),
            ChatResponse::text("已发送。"),
            ChatResponse::text("已发送。"),
            // Turn 2: "就发刚才那封" — the referent resolves to the
            // same mail; the ledger reports already-done, so the
            // model answers that it already went out.
            ChatResponse::calls(vec![call("2", "propose_email", mail_to_boss())]),
            ChatResponse::text("刚才那封已经发出去了，不用重发。"),
            ChatResponse::text("刚才那封已经发出去了，不用重发。"),
        ],
    ));
    let router = Router::single(model);
    let (_r1, sess) = s.run(
        &router,
        AgentConfig::default(),
        "把周报发给 boss@corp.example",
    );
    assert_eq!(s.sent_count(), 1);
    let r2 = s.run_in(&router, AgentConfig::default(), &sess, "就发刚才那封");
    // No duplicate send and no second approval row: the ledger
    // short-circuits the identical action at dispatch.
    assert_eq!(
        s.sent_count(),
        1,
        "identical re-send must not duplicate the mail"
    );
    assert_eq!(s.approvals_with_status("executed"), 1);
    assert_eq!(s.approvals_with_status("pending"), 0);
    assert!(answer(&r2).contains("已经发出"), "{}", answer(&r2));
}

#[test]
fn intent_amount_conflict_asks() {
    // (1) no resource changes; (2) zero dispatch, zero approvals;
    // (3) the answer names both conflicting amounts (1000 vs 1200);
    // (4) one text-only turn.
    let s = Suite::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![ChatResponse::text(
            "金额上限你说的是 1000 元，但参数里写的是 1200 元，以哪个为准？我先不执行。",
        )],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "预算上限 1000 元。按这个参数办：金额 1200 元。",
    );
    assert!(r.tool_calls.is_empty(), "{:?}", r.tool_calls);
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.approvals_with_status("pending"), 0);
    assert_eq!(s.approvals_with_status("executed"), 0);
    let a = answer(&r);
    assert!(a.contains("1000") && a.contains("1200"), "{a}");
}

#[test]
fn intent_ask_user_roundtrip() {
    // (1) the question row settles to Executed (PR #118 fix), not
    //     left Approved; (2) zero sends, zero other approvals;
    // (3) the answer names the chosen option （方案乙） and is not
    //     replaced by a "still waiting" message; (4) one turn.
    // The AskUserExecutor must be registered: turn-end verification
    // reads the question back through it (executor-missing otherwise).
    let s = Suite::new().with_executor(Arc::new(hatch_tools::AskUserExecutor));
    let answerer = {
        let db = &s.db;
        std::thread::scope(|scope| {
            let handle = scope.spawn(move || {
                let q = ApprovalQueue::new(db);
                for _ in 0..200 {
                    if let Ok(rows) = q.list(Some(ApprovalStatus::Pending)) {
                        if let Some(a) = rows.iter().find(|a| a.kind == ASK_KIND) {
                            let now = hatch_core::time::now();
                            answer_question(db, &a.id, Some(Choice::Index(1)), None, &now).unwrap();
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                panic!("question never appeared");
            });
            let model = Arc::new(ScriptedModel::ok(
                "m",
                vec![
                    ChatResponse::calls(vec![call(
                        "1",
                        "use_tool",
                        json!({"name": "ask_user", "arguments": {
                            "question": "用哪种方案？",
                            "options": [
                                {"label": "方案甲", "description": "快"},
                                {"label": "方案乙", "description": "稳"}
                            ],
                            "timeout_secs": 30
                        }}),
                    )]),
                    ChatResponse::text("按方案乙来。"),
                    ChatResponse::text("按方案乙来。"),
                ],
            ));
            let (r, sess) = s.run(
                &Router::single(model),
                AgentConfig::default(),
                "帮我选个方案",
            );
            handle.join().unwrap();
            (r, sess)
        })
    };
    let (r, sess) = answerer;
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.tool_calls, vec!["ask_user"]);
    let result = s.tool_result(&sess, "1");
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["answered"], true, "{result}");
    assert_eq!(v["label"], "方案乙", "{result}");
    assert_eq!(v["choice"], 1, "{result}");
    let rows = ApprovalQueue::new(&s.db).list(None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, ApprovalStatus::Executed);
    let a = answer(&r);
    assert!(a.contains("方案乙"), "{a}");
    assert!(!a.contains("等待审批"), "{a}");
    assert_eq!(s.sent_count(), 0);
}

// ---------------- category 4: policy & approval routing ----------------

#[test]
fn policy_browser_find_tool_discovery() {
    // (1) no browser launches, no side effects; (2) one find_tool
    //     call; (3) the result names the hidden browser_click tool
    //     and browser_open is in the listed set (PR #116 wiring);
    // (4) one short turn.
    let s = Suite::new().with_browser();
    let bridge = ToolBridge::from_specs(allowed_specs(&s.tools, &None));
    assert!(
        bridge.is_listed("browser_open"),
        "browser_open is hot on a default turn"
    );
    assert!(
        bridge.is_hidden("browser_click"),
        "browser_click stays hidden behind find_tool"
    );
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "find_tool",
                json!({"query": "点击页面元素"}),
            )]),
            ChatResponse::text("找到了 browser_click。"),
        ],
    ));
    let (r, sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "帮我点一下页面上的筛选按钮",
    );
    let found = s.tool_result(&sess, "1");
    assert!(found.contains("browser_click"), "{found}");
    assert_eq!(r.tool_calls, vec!["find_tool"]);
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    assert_eq!(s.approvals_with_status("executed"), 0);
}

#[test]
fn policy_judge_unavailable() {
    // (1) the send never executes; (2) zero sends, zero approvals
    //     queued (the gate blocks before queueing);
    // (3) the tool result carries [intent_unavailable], the verdict
    //     row records failure_kind judge_unavailable, and the answer
    //     says the judge was unavailable and nothing was sent;
    // (4) one turn, bounded retry inside the judge budget.
    let s = Suite::new();
    s.seed_person("boss@corp.example", "王总");
    s.seed_grant(ActionClass::SendMessage, &["boss@corp.example"]);
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let model = Arc::new(FnModel::new("m", move |req: &ChatRequest| {
        if req.messages[0]
            .text_content()
            .starts_with("You are an independent intent judge")
        {
            return Err(LlmError::permanent("judge down"));
        }
        let n = turn_calls.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(ChatResponse::calls(vec![call(
                "1",
                "propose_email",
                mail_to_boss(),
            )])),
            _ => Ok(ChatResponse::text(
                "评委不可用，无法确认这封邮件是否符合你的意图，我没有发送。",
            )),
        }
    }));
    let cfg = AgentConfig {
        intent_gate: IntentGateMode::Enforce,
        ..Default::default()
    };
    let (r, sess) = s.run(&Router::single(model), cfg, "把周报发给 boss@corp.example");
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.approvals_with_status("pending"), 0);
    assert_eq!(s.approvals_with_status("executed"), 0);
    let result = s.tool_result(&sess, "1");
    assert!(result.contains("[intent_unavailable]"), "{result}");
    let unavailable: i64 =
        s.db.conn()
            .query_row(
                "SELECT COUNT(*) FROM intent_verdicts WHERE failure_kind = 'judge_unavailable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert!(unavailable >= 1, "judge-unavailable verdict recorded");
    let a = answer(&r);
    assert!(a.contains("评委"), "{a}");
    assert!(a.contains("没有发送"), "{a}");
}
