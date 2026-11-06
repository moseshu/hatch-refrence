//! Tool bridge behaviour: hot tools stay listed, hidden tools are reached
//! through find_tool / use_tool, and a bridged call can never bypass the
//! skill allowlist.

#[path = "common/mod.rs"]
mod common;

use common::{call, Harness};
use hatch_agent::{Agent, AgentConfig, StopReason};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use serde_json::json;
use std::sync::Arc;

#[test]
fn listed_tools_stay_listed_and_hidden_tools_are_hidden() {
    let h = Harness::new();
    let agent = Agent {
        db: &h.db,
        router: &Router::single(Arc::new(ScriptedModel::ok("m", vec![]))),
        tools: &h.tools,
        executors: &h.exec,
        gate: &h.gate,
        config: AgentConfig::default(),
    };
    let names: Vec<String> = agent.tool_specs().iter().map(|s| s.name.clone()).collect();
    // Hot tools from the default registry stay listed, plus the meta tools.
    for hot in [
        "memory_search",
        "memory_add",
        "mail_search",
        "propose_email",
        "calendar_list",
        "propose_event",
    ] {
        assert!(names.contains(&hot.to_string()), "{names:?}");
    }
    assert!(names.contains(&"find_tool".to_string()), "{names:?}");
    assert!(names.contains(&"use_tool".to_string()), "{names:?}");
    // Diagnostic / detail tools are hidden.
    assert!(!names.contains(&"memory_explain".to_string()), "{names:?}");
    assert!(
        !names.contains(&"calendar_free_slots".to_string()),
        "{names:?}"
    );
}

#[test]
fn listed_tool_still_called_directly() {
    let h = Harness::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "memory_search", json!({"query": "x"}))]),
            ChatResponse::text("没有。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), AgentConfig::default(), "查记忆");
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.tool_calls, vec!["memory_search"]);
    assert_eq!(h.tool_result(&s, "1"), "没有找到相关记忆。");
}

#[test]
fn hidden_tool_reached_via_find_and_use() {
    let h = Harness::new();
    let id = h.seed_claim("测试记忆");
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "find_tool",
                json!({"query": "查看记忆的来源"}),
            )]),
            ChatResponse::calls(vec![call(
                "2",
                "use_tool",
                json!({"name": "memory_explain", "arguments": {"id": id}}),
            )]),
            ChatResponse::text("查到了。"),
        ],
    ));
    let (r, s) = h.run(
        Router::single(model),
        AgentConfig::default(),
        "这条记忆哪来的",
    );
    assert_eq!(r.stop, StopReason::Answered);
    // A routed call is reported under the tool it ran.
    assert_eq!(r.tool_calls, vec!["find_tool", "memory_explain"]);
    let found = h.tool_result(&s, "1");
    assert!(found.contains("memory_explain"), "{found}");
    assert!(found.contains("inputSchema"), "{found}");
    assert!(
        h.tool_result(&s, "2").contains("当前条目"),
        "{}",
        h.tool_result(&s, "2")
    );
}

#[test]
fn use_tool_cannot_bypass_the_skill_allowlist() {
    let h = Harness::new();
    let id = h.seed_claim("测试记忆");
    // memory_explain is hidden but allowlisted; mail_search is not allowlisted.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "mail_search", "arguments": {"query": "x"}}),
            )]),
            ChatResponse::calls(vec![call(
                "2",
                "use_tool",
                json!({"name": "memory_explain", "arguments": {"id": id}}),
            )]),
            ChatResponse::text("好了。"),
        ],
    ));
    let cfg = AgentConfig {
        skill_tools: Some(vec!["memory_search".into(), "memory_explain".into()]),
        ..Default::default()
    };
    let (r, s) = h.run(Router::single(model), cfg, "查邮件和记忆");
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.tool_calls, vec!["use_tool", "memory_explain"]);
    let refused = h.tool_result(&s, "1");
    assert!(refused.contains("技能限制"), "{refused}");
    assert!(refused.contains("mail_search"), "{refused}");
    assert!(
        h.tool_result(&s, "2").contains("当前条目"),
        "{}",
        h.tool_result(&s, "2")
    );
    // A listed tool through use_tool is refused too — call it directly.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "memory_search", "arguments": {"query": "x"}}),
            )]),
            ChatResponse::text("好了。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), AgentConfig::default(), "查记忆");
    assert_eq!(r.tool_calls, vec!["use_tool"]);
    assert!(
        h.tool_result(&s, "1").contains("直接调用它"),
        "{}",
        h.tool_result(&s, "1")
    );
    // The direct call is refused the same way as the bridged one, for comparison.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "mail_search", json!({"query": "x"}))]),
            ChatResponse::text("好了。"),
        ],
    ));
    let cfg = AgentConfig {
        skill_tools: Some(vec!["memory_search".into()]),
        ..Default::default()
    };
    let (r, s) = h.run(Router::single(model), cfg, "查邮件");
    assert_eq!(r.tool_calls, vec!["mail_search"]);
    assert!(
        h.tool_result(&s, "1").contains("技能限制"),
        "{}",
        h.tool_result(&s, "1")
    );
}
