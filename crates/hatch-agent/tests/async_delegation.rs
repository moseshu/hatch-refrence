//! Async delegation at the agent level: handoffs are injected into the
//! next turn exactly once, and in-process delegation in async mode
//! falls back to the synchronous path (the eval shape).

use hatch_agent::{
    Agent, AgentConfig, ChildRegistry, Delegation, RunOptions, RunStatus, RunStore, SessionStore, SpawnMode, StopReason,
    TypeRegistry,
};
use hatch_agent::subagent::store::RunResult;
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, Role, Router, ToolCall};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::default_registry;
use hatch_tools::ExecutorRegistry;
use serde_json::json;
use std::sync::{Arc, Mutex};

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args }
}

fn tool_results(r: &ChatRequest) -> usize {
    r.messages.iter().filter(|m| m.role == Role::Tool).count()
}

fn last_tool(r: &ChatRequest) -> String {
    r.messages.iter().rev().find(|m| m.role == Role::Tool).map(|m| m.content.clone()).unwrap_or_default()
}

fn is_child(r: &ChatRequest) -> bool {
    r.messages[0].content.contains("你的角色：子代理")
}

type Seen = Arc<Mutex<Vec<ChatRequest>>>;

fn model(f: impl Fn(&ChatRequest) -> ChatResponse + Send + Sync + 'static) -> (Router, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let m = FnModel::new("routed", move |r| {
        s2.lock().unwrap().push(r.clone());
        Ok(f(r))
    });
    (Router::single(Arc::new(m)), seen)
}

struct Env {
    db: Arc<Db>,
    exec: ExecutorRegistry,
    gate: Gatekeeper,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    Env {
        db: Arc::new(Db::open_in_memory().unwrap()),
        exec: ExecutorRegistry::new(),
        gate: Gatekeeper::new(PolicyConfig::default()),
        _dir: dir,
    }
}

fn async_config(registry: Arc<ChildRegistry>) -> AgentConfig {
    let mut d = Delegation::root(Arc::new(TypeRegistry::builtin()));
    d.spawn = SpawnMode::InProcess;
    d.async_mode = true;
    d.registry = Some(registry);
    AgentConfig { delegation: Some(d), ..Default::default() }
}

fn system_of(r: &ChatRequest) -> &str {
    r.messages.iter().find(|m| m.role == Role::System).map(|m| m.content.as_str()).unwrap_or("")
}

#[test]
fn handoffs_are_injected_once_into_the_next_turn() {
    let e = env();
    let now = hatch_core::time::now();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    // A finished run whose handoff is waiting for delivery.
    let store = RunStore::new(&e.db);
    store
        .create("sa_seed", &format!("session:{s}"), &s, 1, "explore", "查预算", RunStatus::Spawned, &now)
        .unwrap();
    store
        .finish("sa_seed", RunStatus::Completed, &RunResult { answer: "种子结论".into(), ..Default::default() })
        .unwrap();
    store
        .save_handoff(
            "sa_seed",
            "[子代理 sa_seed · 类型 explore · 状态 completed]\n结论：\n种子结论。",
            false,
            &now,
        )
        .unwrap();
    let (router, seen) = model(|_| ChatResponse::text("收到"));
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: async_config(registry),
    };
    let r = agent.run(&s, "你好", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one model call in turn 1");
        assert!(system_of(&seen[0]).contains("子代理交接"), "handoff section present");
        assert!(system_of(&seen[0]).contains("种子结论"), "handoff text injected");
    }
    // Delivered exactly once: the second turn no longer sees it.
    let r = agent.run(&s, "还有事", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one model call in turn 2");
        assert!(!system_of(&seen[1]).contains("种子结论"), "handoff must not repeat");
    }
    assert!(store.pending_handoffs(&format!("session:{s}")).unwrap().is_empty());
}

#[test]
fn async_in_process_delegation_falls_back_to_sync() {
    let e = env();
    let now = hatch_core::time::now();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let (router, _seen) = model(|r| {
        if is_child(r) {
            if tool_results(r) == 0 {
                ChatResponse::calls(vec![call("c1", "memory_search", json!({"query": "预算"}))])
            } else {
                ChatResponse::text("子结论：记忆里没有预算信息。")
            }
        } else if tool_results(r) == 0 {
            ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "查一下记忆里有没有预算信息", "type": "explore"}),
            )])
        } else {
            ChatResponse::text(format!("汇总：{}", last_tool(r).lines().next().unwrap_or("")))
        }
    });
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: async_config(registry),
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent.run(&s, "帮我查预算", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.subagents.len(), 1);
    // The result came back in the SAME turn (sync fallback): the parent
    // quotes the handoff's first line, and no handoff was stored for a
    // later turn.
    assert!(r.answer.as_deref().unwrap().starts_with("汇总：[子代理 sa_"), "{:?}", r.answer);
    let (_, history) = SessionStore::new(&e.db).active(&s).unwrap();
    let handoff = history.iter().find(|m| m.message.content.starts_with("[子代理")).unwrap();
    assert!(handoff.message.content.contains("结论：\n子结论"), "{}", handoff.message.content);
    let run = RunStore::new(&e.db).get(&r.subagents[0]).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert!(RunStore::new(&e.db).get_handoff(&run.id).unwrap().is_none(), "no stored handoff");
}

#[test]
fn tool_specs_describe_the_async_return() {
    let e = env();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let mut d = Delegation::root(Arc::new(TypeRegistry::builtin()));
    d.async_mode = true;
    d.registry = Some(registry);
    let specs = hatch_agent::subagent::tool_specs(&d);
    assert!(specs[0].description.contains("立刻返回 run id"), "{}", specs[0].description);
    assert!(specs[1].description.contains("立刻返回各任务的 run id"), "{}", specs[1].description);
    let sync = Delegation::root(Arc::new(TypeRegistry::builtin()));
    let specs = hatch_agent::subagent::tool_specs(&sync);
    assert!(specs[0].description.contains("等它完成后返回"), "{}", specs[0].description);
}
