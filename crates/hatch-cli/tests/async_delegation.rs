//! Async delegation with real child processes: `delegate` returns the
//! run id at once, the turn ends while the child still runs, and the
//! result comes back as a handoff in a later turn — exactly once.
//! Also covers cancel-by-run-id, the parallel limit, and a crashing
//! child returning a failure handoff.

#[path = "subagent_process/common.rs"]
mod common;
use common::*;

use hatch_agent::{ChildRegistry, RunOptions, RunStatus, RunStore, SessionStore, StopReason};
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, Router};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// The last tool result of the n-th parent model request.
fn tool_of(seen: &Seen, n: usize) -> String {
    use hatch_llm::Role;
    seen.lock().unwrap()[n]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

#[test]
fn delegate_returns_immediately_and_handoff_arrives_next_turn() {
    let e = env();
    let script = write_script(
        e._dir.path(),
        "slow.json",
        json!([{"sleep_secs": 5}, {"text": "子结论：记忆里没有预算信息。"}]),
    );
    let _guard = ScriptGuard::set(&script);
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let now = hatch_core::time::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (router, seen) = model(move |_| {
        match c.fetch_add(1, Ordering::SeqCst) {
            0 => ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "查一下记忆里有没有预算信息", "type": "explore"}),
            )]),
            1 => ChatResponse::text("知道了，子代理在跑。"),
            2 => ChatResponse::text("好的。"),
            3 => ChatResponse::text("收到交接。"),
            _ => ChatResponse::text("确认。"),
        }
    });
    let a = agent(&e, &router, &registry, 60);
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    // Turn 1: delegate and end while the child still sleeps.
    let started = Instant::now();
    let r = a.run(&s, "帮我查预算", &now, RunOptions::default()).unwrap();
    assert!(started.elapsed() < Duration::from_secs(4), "turn 1 must not wait for the child: {:?}", started.elapsed());
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.subagents.len(), 1);
    let run_id = r.subagents[0].clone();
    assert!(tool_of(&seen, 1).contains("已启动子代理"), "delegate returned the run id at once: {}", tool_of(&seen, 1));
    let run = RunStore::new(&e.db).get(&run_id).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Running);
    assert!(registry.contains(&run_id));
    // Turn 2 (child still sleeping): a normal user message is handled,
    // and no handoff is injected yet.
    let r = a.run(&s, "还有别的事", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(!sys(&seen, 2).contains("子结论"), "child is not done yet");
    // The child finishes in the background.
    assert_eq!(wait_terminal(&e.db, &run_id, Duration::from_secs(15)), RunStatus::Completed);
    assert!(!registry.contains(&run_id), "monitor unregistered the run");
    // Turn 3: the handoff arrives.
    let r = a.run(&s, "怎么样了", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(sys(&seen, 3).contains("子代理交接"), "{}", sys(&seen, 3));
    assert!(sys(&seen, 3).contains("子结论：记忆里没有预算信息"), "{}", sys(&seen, 3));
    // Turn 4: delivered exactly once.
    let r = a.run(&s, "确认一下", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(!sys(&seen, 4).contains("子结论"), "handoff must not repeat");
    // Socket + scratch dir are gone with the run.
    assert!(!std::env::temp_dir().join(format!("hatch-sa-{run_id}")).exists());
    assert!(no_subagent_process());
}

#[test]
fn cancel_by_run_id_kills_the_child() {
    let e = env();
    let script = write_script(
        e._dir.path(),
        "slow.json",
        json!([{"sleep_secs": 30}, {"text": "never"}]),
    );
    let _guard = ScriptGuard::set(&script);
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let now = hatch_core::time::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (router, seen) = model(move |_| match c.fetch_add(1, Ordering::SeqCst) {
        0 => ChatResponse::calls(vec![call(
            "p1",
            "delegate",
            json!({"task": "慢慢来", "type": "explore"}),
        )]),
        1 => ChatResponse::text("知道了。"),
        _ => ChatResponse::text("收到。"),
    });
    let a = agent(&e, &router, &registry, 60);
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = a.run(&s, "慢慢来", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let run_id = r.subagents[0].clone();
    assert_eq!(RunStore::new(&e.db).get(&run_id).unwrap().unwrap().status, RunStatus::Running);
    assert!(registry.cancel(&run_id), "cancel must find the run");
    assert_eq!(wait_terminal(&e.db, &run_id, Duration::from_secs(10)), RunStatus::Cancelled);
    let run = RunStore::new(&e.db).get(&run_id).unwrap().unwrap();
    assert!(run.error.contains("取消"), "{}", run.error);
    assert!(no_subagent_process(), "the cancelled child survived");
    // The cancellation itself comes back as a handoff next turn.
    let r = a.run(&s, "怎么样了", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(sys(&seen, 2).contains("cancelled"), "{}", sys(&seen, 2));
}

#[test]
fn cancel_subagent_tool_kills_the_child() {
    let e = env();
    let script = write_script(
        e._dir.path(),
        "slow.json",
        json!([{"sleep_secs": 30}, {"text": "never"}]),
    );
    let _guard = ScriptGuard::set(&script);
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let now = hatch_core::time::now();
    // The run id is only known after turn 1; hand it to the turn-2 model.
    let run_id_cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cell = run_id_cell.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (router, seen) = model(move |_| match c.fetch_add(1, Ordering::SeqCst) {
        0 => ChatResponse::calls(vec![call(
            "p1",
            "delegate",
            json!({"task": "慢慢来", "type": "explore"}),
        )]),
        1 => ChatResponse::text("知道了。"),
        2 => {
            let id = cell
                .lock()
                .unwrap()
                .clone()
                .expect("run id set after turn 1");
            ChatResponse::calls(vec![call("p2", "cancel_subagent", json!({"run_id": id}))])
        }
        3 => ChatResponse::text("已取消。"),
        _ => ChatResponse::text("收到。"),
    });
    let a = agent(&e, &router, &registry, 60);
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = a.run(&s, "慢慢来", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let run_id = r.subagents[0].clone();
    assert_eq!(
        RunStore::new(&e.db).get(&run_id).unwrap().unwrap().status,
        RunStatus::Running
    );
    *run_id_cell.lock().unwrap() = Some(run_id.clone());
    // Turn 2: the model cancels it with the tool — no daemon involved.
    let r = a
        .run(&s, "取消那个子代理", &now, RunOptions::default())
        .unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(
        tool_of(&seen, 3).contains("已取消子代理"),
        "{}",
        tool_of(&seen, 3)
    );
    assert!(r.subagents.contains(&run_id));
    assert_eq!(
        wait_terminal(&e.db, &run_id, Duration::from_secs(10)),
        RunStatus::Cancelled
    );
    assert!(no_subagent_process(), "the cancelled child survived");
    // The cancellation comes back as a handoff next turn.
    let r = a.run(&s, "怎么样了", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(sys(&seen, 4).contains("cancelled"), "{}", sys(&seen, 4));
}

#[test]
fn parallel_limit_rejects_instead_of_queueing() {
    let e = env();
    let script = write_script(
        e._dir.path(),
        "slow.json",
        json!([{"sleep_secs": 5}, {"text": "做完了。"}]),
    );
    let _guard = ScriptGuard::set(&script);
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let now = hatch_core::time::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (router, seen) = model(move |_| {
        match c.fetch_add(1, Ordering::SeqCst) {
            0 => ChatResponse::calls(vec![call(
                "p1",
                "delegate_parallel",
                json!({"tasks": [
                    {"task": "任务一", "type": "explore"},
                    {"task": "任务二", "type": "explore"},
                    {"task": "任务三", "type": "explore"},
                    {"task": "任务四", "type": "explore"}
                ]}),
            )]),
            1 => ChatResponse::text("收到。"),
            2 => ChatResponse::calls(vec![call(
                "p1",
                "delegate_parallel",
                json!({"tasks": [
                    {"task": "任务一", "type": "explore"},
                    {"task": "任务二", "type": "explore"},
                    {"task": "任务三", "type": "explore"}
                ]}),
            )]),
            3 => ChatResponse::text("收到。"),
            4 => ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "任务四", "type": "explore"}),
            )]),
            _ => ChatResponse::text("收到。"),
        }
    });
    let a = agent(&e, &router, &registry, 60);
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    // 4 tasks at once: the whole call fails up front (max_parallel = 3).
    let r = a.run(&s, "并行四个", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(r.subagents.is_empty(), "nothing started: {r:?}");
    assert!(tool_of(&seen, 1).contains("ERROR"), "{}", tool_of(&seen, 1));
    // 3 tasks start; a fourth single delegate while they run is refused.
    let r = a.run(&s, "并行三个", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.subagents.len(), 3);
    assert!(tool_of(&seen, 3).contains("已启动子代理"), "{}", tool_of(&seen, 3));
    let r = a.run(&s, "再来一个", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.subagents.len(), 1, "the rejected run is still recorded");
    let rejected = RunStore::new(&e.db).get(&r.subagents[0]).unwrap().unwrap();
    assert_eq!(rejected.status, RunStatus::Rejected);
    assert!(tool_of(&seen, 5).contains("已达上限"), "{}", tool_of(&seen, 5));
    // Let the three children finish, then nothing is left behind.
    let store = RunStore::new(&e.db);
    for run in store.list(Some(&s), 10).unwrap() {
        if matches!(run.status, RunStatus::Running | RunStatus::Spawned) {
            wait_terminal(&e.db, &run.id, Duration::from_secs(15));
        }
    }
    assert!(no_subagent_process());
}

#[test]
fn crashed_child_returns_a_failure_handoff() {
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
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let now = hatch_core::time::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (router, seen) = model(move |_| match c.fetch_add(1, Ordering::SeqCst) {
        0 => ChatResponse::calls(vec![call(
            "p1",
            "delegate",
            json!({"task": "查一下", "type": "explore"}),
        )]),
        1 => ChatResponse::text("知道了。"),
        _ => ChatResponse::text("收到。"),
    });
    let a = agent(&e, &router, &registry, 60);
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = a.run(&s, "查一下", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let run_id = r.subagents[0].clone();
    assert_eq!(wait_terminal(&e.db, &run_id, Duration::from_secs(10)), RunStatus::Failed);
    // The parent keeps answering: the failure arrives as a handoff.
    let r = a.run(&s, "怎么样了", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(sys(&seen, 2).contains("异常退出"), "{}", sys(&seen, 2));
    assert!(no_subagent_process());
}
