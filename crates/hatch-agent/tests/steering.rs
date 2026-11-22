//! Steering background sub-agents: the `steer_subagent` tool queues
//! messages through the registry, and a child loop injects them
//! (merged into one user message) at its next step boundary.

mod common;

use common::*;
use hatch_agent::subagent::registry::STEER_QUEUE_CAP;
use hatch_agent::subagent::steer::{new_steer_queue, MAX_STEER_CHARS};
use hatch_agent::{
    Agent, AgentConfig, ChildRegistry, RegistryEntry, RunOptions, RunStatus, RunStore, SessionStore,
    StopReason,
};
use hatch_core::CancelToken;
use hatch_llm::{ChatRequest, ChatResponse, Role};
use hatch_tools::builtin::default_registry;
use serde_json::json;
use std::sync::Arc;

fn tool_results(r: &ChatRequest) -> usize {
    r.messages.iter().filter(|m| m.role == Role::Tool).count()
}

fn seed_run(e: &Env, id: &str, parent_ref: &str, session: &str, status: RunStatus) {
    let now = hatch_core::time::now();
    RunStore::new(&e.db)
        .create(id, parent_ref, session, 1, "explore", "查预算", status, &now)
        .unwrap();
}

#[test]
fn steer_tool_queues_and_validates() {
    let e = env();
    let now = hatch_core::time::now();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let parent_ref = format!("session:{s}");
    seed_run(&e, "sa_1", &parent_ref, &s, RunStatus::Running);
    let (entry, mut rx) = RegistryEntry::new(CancelToken::new(), s.clone(), parent_ref.clone());
    assert!(registry.register("sa_1", entry));
    seed_run(&e, "sa_done", &parent_ref, &s, RunStatus::Completed);
    seed_run(&e, "sa_other", "session:other", "other", RunStatus::Running);
    seed_run(&e, "sa_ghost", &parent_ref, &s, RunStatus::Running);
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
    let steer = |args: serde_json::Value| {
        hatch_agent::subagent::handle_steer_call(&agent, &call("c1", "steer_subagent", args), &parent_ref)
    };
    let h = steer(json!({"run_id": "sa_1", "message": "改查交通预算，不要查餐饮"}));
    assert!(h.content.contains("已给子代理 sa_1"), "{}", h.content);
    assert!(h.content.contains("下一步开始前"), "{}", h.content);
    assert_eq!(h.runs, vec!["sa_1"]);
    assert_eq!(rx.try_recv().unwrap(), "改查交通预算，不要查餐饮");
    let h = steer(json!({"run_id": "sa_nope", "message": "x"}));
    assert!(h.content.contains("没有子代理运行"), "{}", h.content);
    let h = steer(json!({"run_id": "sa_done", "message": "x"}));
    assert!(h.content.contains("已经结束"), "{}", h.content);
    let h = steer(json!({"run_id": "sa_other", "message": "x"}));
    assert!(h.content.contains("不属于本次对话"), "{}", h.content);
    let h = steer(json!({"run_id": "sa_ghost", "message": "x"}));
    assert!(h.content.contains("不在本进程运行"), "{}", h.content);
    let h = steer(json!({"message": "x"}));
    assert!(h.content.contains("ERROR"), "{}", h.content);
    let h = steer(json!({"run_id": "sa_1", "message": "  "}));
    assert!(h.content.contains("ERROR"), "{}", h.content);
    let long = "长".repeat(MAX_STEER_CHARS + 1);
    let h = steer(json!({"run_id": "sa_1", "message": long}));
    assert!(h.content.contains("太长"), "{}", h.content);
    assert!(rx.try_recv().is_err(), "refused messages must not be queued");
}

#[test]
fn steer_queue_full_is_refused_not_blocked() {
    let e = env();
    let now = hatch_core::time::now();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let parent_ref = format!("session:{s}");
    seed_run(&e, "sa_1", &parent_ref, &s, RunStatus::Running);
    let (entry, _rx) = RegistryEntry::new(CancelToken::new(), s.clone(), parent_ref.clone());
    assert!(registry.register("sa_1", entry));
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
    for i in 0..STEER_QUEUE_CAP {
        let h = hatch_agent::subagent::handle_steer_call(
            &agent,
            &call("c1", "steer_subagent", json!({"run_id": "sa_1", "message": format!("第{i}条")})),
            &parent_ref,
        );
        assert!(h.content.contains("已给子代理"), "send {i}: {}", h.content);
    }
    let h = hatch_agent::subagent::handle_steer_call(
        &agent,
        &call("c1", "steer_subagent", json!({"run_id": "sa_1", "message": "溢出"})),
        &parent_ref,
    );
    assert!(h.content.contains("队列已满"), "{}", h.content);
}

#[test]
fn steer_injected_merged_at_step_boundary() {
    let e = env();
    let now = hatch_core::time::now();
    let queue = new_steer_queue();
    let q = queue.clone();
    // Step 1: queue two steering messages mid-run (as the socket
    // reader thread would), then call a tool. Step 2 must see both
    // merged into ONE user message.
    let (router, seen) = model(move |r| {
        if tool_results(r) == 0 {
            let mut g = q.lock().unwrap();
            g.push_back("改查交通预算".into());
            g.push_back("结论带上出处".into());
            drop(g);
            ChatResponse::calls(vec![call("c1", "memory_search", json!({"query": "预算"}))])
        } else {
            ChatResponse::text("查完了")
        }
    });
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: AgentConfig { depth: 1, agent_id: "sa_test".into(), steer_queue: Some(queue), ..Default::default() },
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent.run(&s, "查预算", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "two model calls");
    let users: Vec<String> = seen[1]
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.text_content())
        .collect();
    let steered: Vec<&String> = users.iter().filter(|u| u.contains("父代理补充指令")).collect();
    assert_eq!(steered.len(), 1, "both steers merged into one message: {users:?}");
    assert!(steered[0].contains("改查交通预算") && steered[0].contains("结论带上出处"), "{}", steered[0]);
    // The merged message is in the stored transcript exactly once.
    let (_, history) = SessionStore::new(&e.db).active(&s).unwrap();
    let n = history.iter().filter(|m| m.message.text_content().contains("父代理补充指令")).count();
    assert_eq!(n, 1);
}

#[test]
fn no_steer_queue_no_injection() {
    // Ordinary turns (steer_queue None) are untouched by the feature.
    let e = env();
    let now = hatch_core::time::now();
    let (router, seen) = model(|_| ChatResponse::text("好"));
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: AgentConfig::default(),
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent.run(&s, "你好", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let seen = seen.lock().unwrap();
    assert!(!seen[0].messages.iter().any(|m| m.text_content().contains("父代理补充指令")));
}
