//! L0 intent gate (Observe) end-to-end through the agent loop:
//! a bridged `use_tool` call is judged as the inner prepared
//! action, not the `use_tool` shell, and the verdict lands in
//! `intent_verdicts` without affecting dispatch.

#[path = "common/mod.rs"]
mod common;

use common::{call, model, Harness};
use hatch_agent::AgentConfig;
use hatch_llm::{ChatResponse, Role};
use hatch_policy::{ActionClass, Autonomy, PolicyConfig};
use hatch_tools::builtin::default_registry;
use hatch_tools::{IntentGateMode, Tool, ToolContext, ToolOutput};
use serde_json::json;
use std::sync::Arc;

/// A hidden (non-hot) in-scope tool: WriteCalendar, direct.
struct DirectEvent;

impl Tool for DirectEvent {
    fn spec(&self) -> hatch_llm::ToolSpec {
        hatch_llm::ToolSpec {
            name: "direct_event".into(),
            description: "Create a calendar event directly (test stub)".into(),
            parameters: json!({"type": "object", "properties": {"title": {"type": "string"}}}),
        }
    }
    fn class(&self) -> ActionClass {
        ActionClass::WriteCalendar
    }
    fn call(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> hatch_core::Result<ToolOutput> {
        Ok(ToolOutput::system("created"))
    }
}

#[test]
fn bridged_call_is_judged_as_inner_action() {
    let mut policy = PolicyConfig::default();
    policy.set(ActionClass::WriteCalendar, Autonomy::ActNotify);
    let mut h = Harness::with_policy(policy);
    h.tools = default_registry().with(Arc::new(DirectEvent));
    let (router, _seen) = model(|req| {
        if req.json_mode {
            // The intent judge's isolated call.
            return ChatResponse::text(
                "{\"verdict\":\"allow\",\"reason\":\"matches\",\"cited_intent\":\"\",\"cited_field\":\"\"}",
            );
        }
        let seen_tool = req.messages.iter().any(|m| m.role == Role::Tool);
        if seen_tool {
            ChatResponse::text("好了。")
        } else {
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "direct_event", "arguments": {"title": "周会"}}),
            )])
        }
    });
    let cfg = AgentConfig {
        intent_gate: IntentGateMode::Observe,
        ..Default::default()
    };
    let (r, _s) = h.run(router, cfg, "帮我建个周会");
    assert_eq!(r.tool_calls, vec!["direct_event"]);

    let conn = h.db.conn();
    let mut st = conn
        .prepare("SELECT tool, verdict, counterfactual, class FROM intent_verdicts")
        .unwrap();
    let rows: Vec<(String, String, String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    // Judged as the inner action, never as use_tool.
    assert_eq!(rows[0].0, "direct_event");
    assert_eq!(rows[0].1, "allow");
    assert_eq!(rows[0].2, "would_auto_execute");
    assert_eq!(rows[0].3, "write_calendar");
}

#[test]
fn gate_off_records_nothing() {
    let h = Harness::new();
    let (router, _seen) = model(|req| {
        let seen_tool = req.messages.iter().any(|m| m.role == Role::Tool);
        if seen_tool {
            ChatResponse::text("好了。")
        } else {
            ChatResponse::calls(vec![call(
                "1",
                "propose_event",
                json!({"title": "周会", "start": "2026-11-24 10:00"}),
            )])
        }
    });
    let (r, _s) = h.run(router, AgentConfig::default(), "帮我建个明天十点的周会");
    assert_eq!(r.tool_calls, vec!["propose_event"]);
    let n: i64 =
        h.db.conn()
            .query_row("SELECT COUNT(*) FROM intent_verdicts", [], |r| r.get(0))
            .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn direct_call_in_agent_loop_is_judged() {
    // The normal (non-bridged) entry: same review, same record.
    let h = Harness::new();
    let (router, _seen) = model(|req| {
        if req.json_mode {
            return ChatResponse::text(
                "{\"verdict\":\"deny\",\"reason\":\"收件人不对\",\"cited_intent\":\"发给张总\",\"cited_field\":\"to\"}",
            );
        }
        let seen_tool = req.messages.iter().any(|m| m.role == Role::Tool);
        if seen_tool {
            ChatResponse::text("好了。")
        } else {
            ChatResponse::calls(vec![call(
                "1",
                "propose_email",
                json!({"to": ["lisi@corp.com"], "subject": "周报", "body": "内容"}),
            )])
        }
    });
    let cfg = AgentConfig {
        intent_gate: IntentGateMode::Observe,
        ..Default::default()
    };
    let (r, s) = h.run(router, cfg, "把周报发给张总");
    assert_eq!(r.tool_calls, vec!["propose_email"]);
    // Observe: the deny verdict is recorded, dispatch unaffected
    // (the mail still queued for approval as usual).
    assert!(
        h.tool_result(&s, "1").contains("已提交审批"),
        "{}",
        h.tool_result(&s, "1")
    );
    let conn = h.db.conn();
    let (tool, verdict): (String, String) = conn
        .query_row("SELECT tool, verdict FROM intent_verdicts", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(tool, "send_email");
    assert_eq!(verdict, "deny");
}
