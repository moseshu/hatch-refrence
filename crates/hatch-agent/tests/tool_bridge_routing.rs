//! Tool bridge routing edge cases: no-match and shape errors, names judged
//! by name, gate parity between bridged and direct calls, and the allowlist
//! narrowing the bridge itself.

#[path = "common/mod.rs"]
mod common;

use common::{call, Harness};
use hatch_agent::{Agent, AgentConfig};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use hatch_policy::{ActionClass, Autonomy, PolicyConfig};
use serde_json::json;
use std::sync::Arc;

#[test]
fn find_tool_no_match_lists_names_and_listed_name_is_direct() {
    let h = Harness::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "find_tool", json!({"query": "zzzqqq"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "find_tool",
                json!({"query": "memory_search"}),
            )]),
            ChatResponse::calls(vec![call("3", "find_tool", json!({}))]),
            ChatResponse::text("好了。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), AgentConfig::default(), "找工具");
    assert_eq!(r.tool_calls, vec!["find_tool", "find_tool", "find_tool"]);
    let no_match = h.tool_result(&s, "1");
    let v: serde_json::Value = serde_json::from_str(&no_match).unwrap();
    assert_eq!(v["matches"].as_array().unwrap().len(), 0);
    assert!(
        v["next"].as_str().unwrap().contains("memory_explain"),
        "{no_match}"
    );
    assert!(
        h.tool_result(&s, "2").contains("在工具列表里"),
        "{}",
        h.tool_result(&s, "2")
    );
    assert!(
        h.tool_result(&s, "3").contains("find_tool 需要"),
        "{}",
        h.tool_result(&s, "3")
    );
}

#[test]
fn use_tool_shape_errors_and_unknown_tools_match_direct_calls() {
    let h = Harness::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "use_tool", json!({}))]),
            ChatResponse::calls(vec![call(
                "2",
                "use_tool",
                json!({"name": "nope_tool", "arguments": {}}),
            )]),
            ChatResponse::calls(vec![call("3", "nope_tool", json!({}))]),
            ChatResponse::text("好了。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), AgentConfig::default(), "乱调工具");
    assert_eq!(r.tool_calls, vec!["use_tool", "use_tool", "nope_tool"]);
    assert!(
        h.tool_result(&s, "1").contains("use_tool 需要"),
        "{}",
        h.tool_result(&s, "1")
    );
    // An unknown name is passed on, so the one unknown-tool check refuses
    // it exactly as it would a direct call.
    assert_eq!(h.tool_result(&s, "2"), h.tool_result(&s, "3"));
    assert!(
        h.tool_result(&s, "2").contains("unknown tool"),
        "{}",
        h.tool_result(&s, "2")
    );
}

#[test]
fn use_tool_refuses_names_judged_by_name() {
    let h = Harness::new();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "delegate", "arguments": {"task": "x", "type": "explore"}}),
            )]),
            ChatResponse::calls(vec![call(
                "2",
                "use_tool",
                json!({"name": "find_tool", "arguments": {"query": "x"}}),
            )]),
            ChatResponse::text("好了。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), AgentConfig::default(), "乱调工具");
    assert_eq!(r.tool_calls, vec!["use_tool", "use_tool"]);
    assert!(
        h.tool_result(&s, "1").contains("直接调用它"),
        "{}",
        h.tool_result(&s, "1")
    );
    assert!(
        h.tool_result(&s, "2").contains("直接调用它"),
        "{}",
        h.tool_result(&s, "2")
    );
}

#[test]
fn bridged_call_is_judged_like_a_direct_call() {
    // The gate denies ReadPersonal outright: a bridged memory_explain must
    // be refused with exactly the same text as a direct one.
    let mut policy = PolicyConfig::default();
    policy.set(ActionClass::ReadPersonal, Autonomy::Suggest);
    let h = Harness::with_policy(policy);
    let id = h.seed_claim("测试记忆");
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "memory_explain", "arguments": {"id": id}}),
            )]),
            ChatResponse::calls(vec![call("2", "memory_explain", json!({"id": id}))]),
            ChatResponse::text("好了。"),
        ],
    ));
    let (r, s) = h.run(
        Router::single(model),
        AgentConfig::default(),
        "这条记忆哪来的",
    );
    assert_eq!(r.tool_calls, vec!["memory_explain", "memory_explain"]);
    assert!(
        h.tool_result(&s, "1").contains("被策略拒绝"),
        "{}",
        h.tool_result(&s, "1")
    );
    assert_eq!(h.tool_result(&s, "1"), h.tool_result(&s, "2"));
}

#[test]
fn allowlist_narrows_the_bridge() {
    let h = Harness::new();
    let cfg = AgentConfig {
        skill_tools: Some(vec!["memory_search".into()]),
        ..Default::default()
    };
    let agent = Agent {
        db: &h.db,
        router: &Router::single(Arc::new(ScriptedModel::ok("m", vec![]))),
        tools: &h.tools,
        executors: &h.exec,
        gate: &h.gate,
        config: cfg.clone(),
    };
    let names: Vec<String> = agent.tool_specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, ["memory_search", "find_tool", "use_tool"]);
    // Nothing is hidden, so find_tool finds nothing.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "find_tool",
                json!({"query": "查看记忆的来源"}),
            )]),
            ChatResponse::text("好了。"),
        ],
    ));
    let (r, s) = h.run(Router::single(model), cfg, "找工具");
    assert_eq!(r.tool_calls, vec!["find_tool"]);
    let v: serde_json::Value = serde_json::from_str(&h.tool_result(&s, "1")).unwrap();
    assert_eq!(v["matches"].as_array().unwrap().len(), 0);
}
