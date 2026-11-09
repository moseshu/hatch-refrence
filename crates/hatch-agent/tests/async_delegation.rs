//! Async delegation at the agent level: handoffs are injected into the
//! next turn exactly once, and in-process delegation in async mode
//! falls back to the synchronous path (the eval shape).

mod common;

use common::*;
use hatch_agent::subagent::store::RunResult;
use hatch_agent::{
    Agent, ChildRegistry, Delegation, RegistryEntry, RunOptions, RunStatus, RunStore, SessionStore,
    StopReason, TypeRegistry,
};
use hatch_core::CancelToken;
use hatch_llm::{ChatRequest, ChatResponse, Role};
use hatch_tools::builtin::default_registry;
use serde_json::json;
use std::sync::Arc;

fn tool_results(r: &ChatRequest) -> usize {
    r.messages.iter().filter(|m| m.role == Role::Tool).count()
}

fn last_tool(r: &ChatRequest) -> String {
    r.messages.iter().rev().find(|m| m.role == Role::Tool).map(|m| m.content.clone()).unwrap_or_default()
}

fn is_child(r: &ChatRequest) -> bool {
    r.messages[0].content.contains("你的角色：子代理")
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
fn cancel_spec_only_in_async_main_turns() {
    let e = env();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    // Async main agent: cancel is offered alongside delegate.
    let mut d = Delegation::root(Arc::new(TypeRegistry::builtin()));
    d.async_mode = true;
    d.registry = Some(registry.clone());
    let names: Vec<String> = hatch_agent::subagent::tool_specs(&d)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(
        names,
        vec!["delegate", "delegate_parallel", "cancel_subagent", "pause_subagent", "resume_subagent"]
    );
    // Synchronous: nothing to cancel, the tool is not offered.
    let sync = Delegation::root(Arc::new(TypeRegistry::builtin()));
    let names: Vec<String> = hatch_agent::subagent::tool_specs(&sync)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["delegate", "delegate_parallel"]);
    // A child agent (depth 1) never has a background run to cancel.
    let mut child = Delegation::root(Arc::new(TypeRegistry::builtin()));
    child.depth = 1;
    child.async_mode = true;
    child.registry = Some(registry);
    let names: Vec<String> = hatch_agent::subagent::tool_specs(&child)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["delegate", "delegate_parallel"]);
}

#[test]
fn cancel_subagent_tool_cancels_own_run() {
    let e = env();
    let now = hatch_core::time::now();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let parent_ref = format!("session:{s}");
    let store = RunStore::new(&e.db);
    // A running child owned by this process.
    store
        .create(
            "sa_1",
            &parent_ref,
            &s,
            1,
            "explore",
            "查预算",
            RunStatus::Running,
            &now,
        )
        .unwrap();
    let token = CancelToken::new();
    assert!(registry.register(
        "sa_1",
        RegistryEntry {
            cancel: token.clone(),
            session: s.clone(),
            parent_ref: parent_ref.clone()
        }
    ));
    // A finished run, a foreign run, and a run owned by another process.
    store
        .create(
            "sa_done",
            &parent_ref,
            &s,
            1,
            "explore",
            "已做完",
            RunStatus::Completed,
            &now,
        )
        .unwrap();
    store
        .create("sa_other", "session:other", "other", 1, "explore", "别人的", RunStatus::Running, &now)
        .unwrap();
    store
        .create("sa_ghost", &parent_ref, &s, 1, "explore", "不在本进程", RunStatus::Running, &now)
        .unwrap();
    let (router, _seen) = model(|_| ChatResponse::text("收到"));
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: async_config(registry),
    };
    let cancel = |id: &str| {
        hatch_agent::subagent::handle_cancel_call(
            &agent,
            &call("c1", "cancel_subagent", json!({"run_id": id})),
            &parent_ref,
        )
    };
    let h = cancel("sa_1");
    assert!(h.content.contains("已取消子代理 sa_1"), "{}", h.content);
    assert!(token.is_cancelled());
    assert_eq!(h.runs, vec!["sa_1"]);
    let h = cancel("sa_nope");
    assert!(h.content.contains("没有子代理运行"), "{}", h.content);
    let h = cancel("sa_done");
    assert!(h.content.contains("已经结束"), "{}", h.content);
    assert!(h.content.contains("completed"), "{}", h.content);
    let h = cancel("sa_other");
    assert!(h.content.contains("不属于本次对话"), "{}", h.content);
    let h = cancel("sa_ghost");
    assert!(h.content.contains("不在本进程运行"), "{}", h.content);
    let h = hatch_agent::subagent::handle_cancel_call(
        &agent,
        &call("c1", "cancel_subagent", json!({})),
        &parent_ref,
    );
    assert!(h.content.contains("ERROR"), "{}", h.content);
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
