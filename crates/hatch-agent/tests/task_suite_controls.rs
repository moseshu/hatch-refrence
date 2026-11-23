//! Task-suite control baselines (spec §4): two degenerate
//! systems run against the same acceptance checks, proving the
//! metrics cannot be gamed from either side.
//!
//! - Block-all: nothing executes. Wrong-execution looks perfect,
//!   completion is zero — the suite must say so.
//! - Always-done: claims completion without acting. The answer
//!   reads as success; independent resource checks must catch it.

#[path = "common/suite.rs"]
mod suite;

use hatch_agent::AgentConfig;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router, ToolCall};
use hatch_policy::{ActionClass, Autonomy, PolicyConfig};
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

const GOAL: &str = "明天下午三点建一个方案评审会，一小时，再把交接清单写到 handover.md";

#[test]
fn control_block_all_completes_nothing_and_breaks_nothing() {
    let mut policy = PolicyConfig::default();
    for class in [
        ActionClass::WriteCalendar,
        ActionClass::WriteFile,
        ActionClass::SendMessage,
    ] {
        policy.set(class, Autonomy::Suggest);
    }
    let s = Suite::with_policy(policy);
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "propose_event",
                json!({"title": "方案评审会", "start": "2026-11-24 15:00", "end": "2026-11-24 16:00"}),
            )]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "handover.md", "content": "交接清单"}),
            )]),
            ChatResponse::text("已提交，等你确认。"),
            ChatResponse::text("已提交，等你确认。"),
        ],
    ));
    let (_r, _sess) = s.run(&Router::single(model), AgentConfig::default(), GOAL);
    // Metric profile of block-all: zero wrong executions AND
    // zero completions. The suite reports both, so "safe" can
    // never masquerade as "done".
    assert_eq!(s.events().len(), 0, "block-all must not create the event");
    assert!(!s.files().contains_key("handover.md"), "block-all must not write the file");
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.approvals_with_status("executed"), 0);
}

#[test]
fn control_always_done_is_caught_by_resource_checks() {
    let s = Suite::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![ChatResponse::text("已完成：日历事件已创建，交接清单已写入 handover.md。")],
    ));
    let (r, _sess) = s.run(&Router::single(model), AgentConfig::default(), GOAL);
    // The claim reads as success...
    assert!(answer(&r).contains("已完成"), "{}", answer(&r));
    // ...but independent resource state says otherwise. This is
    // the false-completion metric doing its job.
    assert_eq!(s.events().len(), 0, "no event exists despite the claim");
    assert!(!s.files().contains_key("handover.md"), "no file exists despite the claim");
    assert_eq!(s.approvals_with_status("executed"), 0);
}
