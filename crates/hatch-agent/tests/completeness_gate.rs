//! Turn-end completeness self-check: after a side-effecting turn the
//! model gets exactly one intercepted finish — shown its own executed
//! actions against the user's request — and may remediate. Read-only
//! and tool-free turns are never intercepted.

use hatch_agent::{Agent, AgentConfig, SessionStore, StopReason};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Role, Router, ToolCall};
use hatch_memory::MemoryStore;
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::default_registry;
use hatch_tools::ExecutorRegistry;
use serde_json::json;
use std::sync::Arc;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args }
}

fn add(id: &str, text: &str) -> ChatResponse {
    ChatResponse::calls(vec![call(
        id,
        "memory_add",
        json!({"subject": "user", "text": text}),
    )])
}

struct Rig {
    db: Db,
    exec: ExecutorRegistry,
    gate: Gatekeeper,
    tools: hatch_tools::Registry,
}

fn rig() -> Rig {
    Rig {
        db: Db::open_in_memory().unwrap(),
        exec: ExecutorRegistry::new(),
        gate: Gatekeeper::new(PolicyConfig::default()),
        tools: default_registry(),
    }
}

fn run(rig: &Rig, model: Arc<ScriptedModel>, cfg: AgentConfig, input: &str) -> hatch_agent::TurnReport {
    let router = Router::single(model);
    let agent = Agent {
        db: &rig.db,
        router: &router,
        tools: &rig.tools,
        executors: &rig.exec,
        gate: &rig.gate,
        config: cfg,
    };
    let now = hatch_core::time::now();
    let s = SessionStore::new(&rig.db).create("t", &now).unwrap();
    agent.run_turn(&s, input, &now).unwrap()
}

/// All user-role message texts across every request the model saw.
fn user_texts(model: &ScriptedModel) -> Vec<String> {
    model
        .requests()
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter(|m| m.role == Role::User)
        .map(|m| m.text_content())
        .collect()
}

/// Check notes present in the final request's history. The note is
/// appended to the transcript once and then rides along in every
/// later request, so count it in the last request only.
fn check_notes(model: &ScriptedModel) -> usize {
    let reqs = model.requests();
    let Some(last) = reqs.last() else { return 0 };
    last.messages
        .iter()
        .filter(|m| m.role == Role::User && m.text_content().contains("System self-check"))
        .count()
}

#[test]
fn gate_intercepts_first_finish_and_model_remediates() {
    let rig = rig();
    // Model remembers only A, tries to finish; after the check it
    // remembers B too, then answers.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            add("1", "事实A"),
            ChatResponse::text("记好了"),
            add("2", "事实B"),
            ChatResponse::text("都记好了"),
        ],
    ));
    let r = run(
        &rig,
        model.clone(),
        AgentConfig::default(),
        "记住事实A和事实B",
    );
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.answer.as_deref(), Some("都记好了"));
    assert_eq!(check_notes(&model), 1, "check injected exactly once");
    // The check message must carry the request and the executed action.
    let note = user_texts(&model)
        .into_iter()
        .find(|t| t.contains("System self-check"))
        .unwrap();
    assert!(note.contains("记住事实A和事实B"), "{note}");
    assert!(note.contains("memory_add"), "{note}");
    // Both facts landed in the bank.
    let store = MemoryStore::new(&rig.db);
    let now = hatch_core::time::now();
    assert!(!store.search("事实A", 5, &now).unwrap().is_empty());
    assert!(!store.search("事实B", 5, &now).unwrap().is_empty());
}

#[test]
fn gate_fires_at_most_once_per_turn() {
    let rig = rig();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            add("1", "事实A"),
            ChatResponse::text("记好了"),
            ChatResponse::text("确认无遗漏"),
        ],
    ));
    let r = run(&rig, model.clone(), AgentConfig::default(), "记住事实A");
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.answer.as_deref(), Some("确认无遗漏"));
    assert_eq!(check_notes(&model), 1);
    assert_eq!(model.requests().len(), 3);
}

#[test]
fn read_only_turn_is_not_intercepted() {
    let rig = rig();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "memory_search", json!({"query": "x"}))]),
            ChatResponse::text("查完了"),
        ],
    ));
    let r = run(&rig, model.clone(), AgentConfig::default(), "查一下x");
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.answer.as_deref(), Some("查完了"));
    assert_eq!(check_notes(&model), 0);
    assert_eq!(model.requests().len(), 2);
}

#[test]
fn tool_free_turn_is_not_intercepted() {
    let rig = rig();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("你好")]));
    let r = run(&rig, model.clone(), AgentConfig::default(), "你好");
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(check_notes(&model), 0);
    assert_eq!(model.requests().len(), 1);
}

#[test]
fn gate_skips_when_step_budget_too_tight() {
    let rig = rig();
    // max_steps=3: after the write (step 0) and the finish attempt
    // (step 1) only the forced text-only last step remains — no room
    // to remediate, so the check must not fire.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![add("1", "事实A"), ChatResponse::text("记好了")],
    ));
    let cfg = AgentConfig { max_steps: 3, ..Default::default() };
    let r = run(&rig, model.clone(), cfg, "记住事实A");
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.answer.as_deref(), Some("记好了"));
    assert_eq!(check_notes(&model), 0);
    assert_eq!(model.requests().len(), 2);
}

#[test]
fn gate_can_be_disabled_by_config() {
    let rig = rig();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![add("1", "事实A"), ChatResponse::text("记好了")],
    ));
    let cfg = AgentConfig { completeness_check: false, ..Default::default() };
    let r = run(&rig, model.clone(), cfg, "记住事实A");
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(check_notes(&model), 0);
    assert_eq!(model.requests().len(), 2);
}
