//! Sub-agents as child processes: completion, timeout, and crash — the
//! parent keeps answering and records the handoff.

#[path = "subagent_process/common.rs"]
mod common;
use common::*;

use hatch_agent::{Agent, RunOptions, RunStatus, RunStore, SessionStore, StopReason};
use hatch_llm::ChatResponse;
use hatch_tools::builtin::default_registry;
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn process_child_completes_and_reports_back() {
    let e = env();
    let script = write_script(
        e._dir.path(),
        "ok.json",
        json!([
            {"tool_calls": [{"id": "c1", "name": "memory_search", "arguments": {"query": "预算"}}]},
            {"text": "子结论：记忆里没有预算信息。"}
        ]),
    );
    let _guard = ScriptGuard::set(&script);
    let now = hatch_core::time::now();
    let router = model(|r| {
        if tool_results(r) == 0 {
            ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "查一下记忆里有没有预算信息", "type": "explore"}),
            )])
        } else {
            ChatResponse::text(format!(
                "汇总：{}",
                last_tool(r).lines().next().unwrap_or("")
            ))
        }
    });
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: proc_config(&e, 60),
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent
        .run(&s, "帮我查预算", &now, RunOptions::default())
        .unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.subagents.len(), 1);
    // The parent's summary quotes the handoff's first line (the header);
    // the child's conclusion is checked in the stored handoff below.
    assert!(
        r.answer
            .as_deref()
            .unwrap()
            .starts_with("汇总：[子代理 sa_"),
        "{:?}",
        r.answer
    );
    let run = RunStore::new(&e.db).get(&r.subagents[0]).unwrap().unwrap();
    assert_eq!(
        (run.status, run.depth, run.agent_type.as_str()),
        (RunStatus::Completed, 1, "explore")
    );
    assert_eq!(run.model_calls, 2);
    let (_, history) = SessionStore::new(&e.db).active(&s).unwrap();
    let handoff = history
        .iter()
        .find(|m| m.message.content.starts_with("[子代理"))
        .unwrap();
    assert!(handoff.message.content.contains("结论：\n子结论"));
    // The child never wrote to the shared database: only the parent
    // session exists there, and its transcript has no child session.
    let sessions = SessionStore::new(&e.db).list(false, 10).unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(!sessions[0].title.starts_with("subagent:"));
    // Scratch dir removed after the run.
    assert!(!std::env::temp_dir()
        .join(format!("hatch-sa-{}", r.subagents[0]))
        .exists());
    assert!(no_subagent_process());
}

#[test]
fn process_child_timeout_is_enforced() {
    let e = env();
    let script = write_script(
        e._dir.path(),
        "slow.json",
        json!([{"sleep_secs": 30}, {"text": "never"}]),
    );
    let _guard = ScriptGuard::set(&script);
    let now = hatch_core::time::now();
    let router = model(|r| {
        if tool_results(r) == 0 {
            ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "慢慢来", "type": "explore"}),
            )])
        } else {
            ChatResponse::text("收到")
        }
    });
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: proc_config(&e, 3),
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let started = Instant::now();
    let r = agent
        .run(&s, "慢慢来", &now, RunOptions::default())
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(
        r.stop,
        StopReason::Answered,
        "the parent still answers after the timeout"
    );
    let run = RunStore::new(&e.db).get(&r.subagents[0]).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.contains("超过 3 秒"), "{}", run.error);
    assert!(no_subagent_process(), "the timed-out child survived");
}

#[test]
fn process_child_crash_does_not_kill_parent() {
    let e = env();
    let script = write_script(
        e._dir.path(),
        "crash.json",
        json!([
            {"tool_calls": [{"id": "c1", "name": "memory_search", "arguments": {"query": "x"}}]},
            {"exit": 1}
        ]),
    );
    let _guard = ScriptGuard::set(&script);
    let now = hatch_core::time::now();
    let router = model(|r| {
        if tool_results(r) == 0 {
            ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "查一下", "type": "explore"}),
            )])
        } else {
            ChatResponse::text("收到")
        }
    });
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: proc_config(&e, 60),
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent
        .run(&s, "查一下", &now, RunOptions::default())
        .unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let run = RunStore::new(&e.db).get(&r.subagents[0]).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.contains("异常退出"), "{}", run.error);
    let (_, history) = SessionStore::new(&e.db).active(&s).unwrap();
    let handoff = history
        .iter()
        .find(|m| m.message.content.starts_with("[子代理"))
        .unwrap();
    assert!(handoff.message.content.contains("异常退出"));
    assert!(no_subagent_process());
}
