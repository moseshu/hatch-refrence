//! `todo_write`: the main agent's live progress checklist.

mod common;

use common::{call, env, model};
use hatch_agent::{Agent, AgentConfig, Progress, ProgressKind, RunOptions, SessionStore};
use hatch_llm::ChatResponse;
use hatch_tools::builtin::default_registry;
use serde_json::json;
use std::sync::Mutex;

fn tool_results(r: &hatch_llm::ChatRequest) -> usize {
    r.messages
        .iter()
        .filter(|m| m.role == hatch_llm::Role::Tool)
        .count()
}

fn last_tool(r: &hatch_llm::ChatRequest) -> String {
    r.messages
        .iter()
        .rev()
        .find(|m| m.role == hatch_llm::Role::Tool)
        .map(|m| m.text_content())
        .unwrap_or_default()
}

#[test]
fn spec_offered_to_main_agent_only() {
    let e = env();
    let (router, _seen) = model(|_| ChatResponse::text("ok"));
    let tools = default_registry();
    let names = |config: AgentConfig| -> Vec<String> {
        let agent = Agent {
            db: &e.db,
            router: &router,
            tools: &tools,
            executors: &e.exec,
            gate: &e.gate,
            config,
        };
        agent.tool_specs().iter().map(|t| t.name.clone()).collect()
    };
    assert!(names(AgentConfig::default()).contains(&"todo_write".to_string()));
    assert!(!names(AgentConfig {
        todo_tools: false,
        ..Default::default()
    })
    .contains(&"todo_write".to_string()));
    assert!(!names(AgentConfig {
        depth: 1,
        ..Default::default()
    })
    .contains(&"todo_write".to_string()));
}

#[test]
fn write_emits_progress_echoes_and_audits() {
    let e = env();
    let now = hatch_core::time::now();
    let (router, _seen) = model(|r| {
        if tool_results(r) == 0 {
            ChatResponse::calls(vec![call(
                "t1",
                "todo_write",
                json!({"todos": [
                    {"content": "写测试", "status": "in_progress", "activeForm": "正在写测试"},
                    {"content": "跑回归", "status": "pending"},
                ]}),
            )])
        } else {
            ChatResponse::text("好。")
        }
    });
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
    let events = Mutex::new(Vec::<Progress>::new());
    let observe = |p: &Progress| events.lock().unwrap().push(p.clone());
    let r = agent
        .run(
            &s,
            "做两件事",
            &now,
            RunOptions {
                observer: Some(&observe),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(r.stop, hatch_agent::StopReason::Answered);
    let ev = events.lock().unwrap();
    let todo = ev
        .iter()
        .find(|p| p.kind == ProgressKind::Todo)
        .expect("todo progress event");
    assert!(todo.detail.contains("0/2 完成"), "{}", todo.detail);
    assert!(todo.detail.contains("正在写测试"), "{}", todo.detail);
    let audit = e.db.audit_tail(10).unwrap();
    assert!(
        audit.iter().any(|a| a.event == "todo_write" && a.detail == "n=2 done=0"),
        "{audit:?}"
    );
}

#[test]
fn key_repair_and_error_paths() {
    let e = env();
    let now = hatch_core::time::now();
    let (router, seen) = model(|r| match tool_results(r) {
        0 => ChatResponse::calls(vec![call(
            "t1",
            "todo_write",
            json!({"items": [
                {"subject": "第一步", "state": "done"},
                {"text": "第二步", "status": "doing", "active_form": "正在做第二步"},
            ]}),
        )]),
        1 => ChatResponse::calls(vec![call(
            "t2",
            "todo_write",
            json!({"todos": [{"content": "x", "status": "blocked"}]}),
        )]),
        _ => ChatResponse::text("完。"),
    });
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
    let events = Mutex::new(Vec::<Progress>::new());
    let observe = |p: &Progress| events.lock().unwrap().push(p.clone());
    agent
        .run(
            &s,
            "两步",
            &now,
            RunOptions {
                observer: Some(&observe),
                ..Default::default()
            },
        )
        .unwrap();
    // Repaired keys produced a progress event; the bad status did not.
    let ev = events.lock().unwrap();
    let todos: Vec<&Progress> = ev.iter().filter(|p| p.kind == ProgressKind::Todo).collect();
    assert_eq!(todos.len(), 1, "{ev:?}");
    assert!(todos[0].detail.contains("1/2 完成"), "{}", todos[0].detail);
    let seen = seen.lock().unwrap();
    let err = last_tool(seen.last().unwrap());
    assert!(err.starts_with("ERROR:"), "{err}");
    assert!(err.contains("pending / in_progress / completed"), "{err}");
}

#[test]
fn all_completed_notes_cleared_and_allowlist_exempts() {
    let e = env();
    let now = hatch_core::time::now();
    let (router, seen) = model(|r| {
        if tool_results(r) == 0 {
            ChatResponse::calls(vec![call(
                "t1",
                "todo_write",
                json!({"todos": [{"content": "唯一一步", "status": "completed"}]}),
            )])
        } else {
            ChatResponse::text("完。")
        }
    });
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: AgentConfig {
            // A skill allowlist that does not name todo_write must not
            // block it: it is a progress meta-tool.
            skill_tools: Some(vec!["web_search".into()]),
            ..Default::default()
        },
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    agent.run_turn(&s, "一步", &now).unwrap();
    let seen = seen.lock().unwrap();
    let result = last_tool(seen.last().unwrap());
    assert!(result.contains("已清空"), "{result}");
}
