//! D5 pause/resume: the run record's transitions, the registry's
//! process-level stop/continue, and reconciliation of paused runs.
//! These live here (not inline in `registry.rs` / `store.rs`) so the
//! implementation files stay under the 400-line limit.

mod common;

use common::*;
use hatch_agent::subagent::reconcile_runs;
use hatch_agent::subagent::store::RunResult;
use hatch_agent::{Agent, ChildRegistry, ControlOutcome, Delegation, RegistryEntry, RunStatus, RunStore, SessionStore, TypeRegistry};
use hatch_core::CancelToken;
use hatch_llm::ChatResponse;
use hatch_store::Db;
use hatch_tools::builtin::default_registry;
use serde_json::json;
use std::sync::Arc;

fn entry(session: &str, parent_ref: &str) -> RegistryEntry {
    RegistryEntry { cancel: CancelToken::new(), session: session.to_string(), parent_ref: parent_ref.to_string() }
}

fn seed_running(db: &Db, id: &str, pid: u32) {
    let store = RunStore::new(db);
    let now = hatch_core::time::now();
    store.create(id, "session:s", "s", 1, "explore", "可暂停的任务", RunStatus::Running, &now).unwrap();
    store.set_child_pid(id, pid).unwrap();
}

#[test]
fn pause_and_resume_only_move_between_running_and_paused() {
    let db = Db::open_in_memory().unwrap();
    let s = RunStore::new(&db);
    let t = hatch_core::time::now();
    s.create("sa_1", "session:s", "s", 1, "explore", "任务", RunStatus::Running, &t).unwrap();
    assert!(!s.resume("sa_1").unwrap(), "resume moves only from paused");
    assert!(s.pause("sa_1").unwrap());
    assert_eq!(s.get("sa_1").unwrap().unwrap().status, RunStatus::Paused);
    assert!(s.get("sa_1").unwrap().unwrap().finished_at.is_none(), "pausing is not finishing");
    assert!(!s.pause("sa_1").unwrap(), "a second pause is a no-op");
    assert!(s.resume("sa_1").unwrap());
    assert_eq!(s.get("sa_1").unwrap().unwrap().status, RunStatus::Running);
    assert!(!s.resume("sa_1").unwrap(), "a second resume is a no-op");
    assert!(!s.pause("sa_nope").unwrap() && !s.resume("sa_nope").unwrap());
    s.finish("sa_1", RunStatus::Completed, &RunResult { answer: "完".into(), ..Default::default() }).unwrap();
    assert!(!s.pause("sa_1").unwrap() && !s.resume("sa_1").unwrap(), "terminal runs never move again");
    // Paused runs are listed apart from running ones.
    s.create("sa_2", "session:s", "s", 1, "explore", "任务二", RunStatus::Running, &t).unwrap();
    s.pause("sa_2").unwrap();
    assert_eq!(s.paused_for_parent("session:s").unwrap().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["sa_2"]);
    assert!(s.running_for_parent("session:s").unwrap().is_empty());
}

#[test]
fn pause_freezes_a_real_process_and_resume_continues_the_same_run() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let reg = ChildRegistry::new(Arc::clone(&db));
    let (sleeper, pid) = LiveSleeper::spawn();
    let _guard = sleeper;
    seed_running(&db, "sa_1", pid);
    let token = CancelToken::new();
    assert!(reg.register(
        "sa_1",
        RegistryEntry { cancel: token.clone(), session: "s".into(), parent_ref: "session:s".into() }
    ));
    assert!(reg.contains("sa_1"));
    // The run is still counted while paused: it holds a process, a
    // socket and a slot, and resuming must not over-commit.
    assert_eq!(reg.pause("sa_1"), ControlOutcome::Done);
    assert!(wait_state(pid, "T"), "a paused process reports state T");
    assert_eq!(RunStore::new(&db).get("sa_1").unwrap().unwrap().status, RunStatus::Paused);
    assert!(reg.is_paused("sa_1"));
    assert_eq!(reg.running_for("session:s"), 1);
    // A second pause is an idempotent no-op, not an error.
    assert_eq!(reg.pause("sa_1"), ControlOutcome::Already);
    assert!(wait_state(pid, "T"), "a repeat pause leaves the process stopped");
    assert_eq!(reg.resume("sa_1"), ControlOutcome::Done);
    assert!(wait_not_state(pid, "T"), "a resumed process leaves state T");
    assert_eq!(RunStore::new(&db).get("sa_1").unwrap().unwrap().status, RunStatus::Running);
    assert!(!reg.is_paused("sa_1"));
    // A second resume is an idempotent no-op too.
    assert_eq!(reg.resume("sa_1"), ControlOutcome::Already);
    // Pause then cancel: the cancel token fires (the monitor thread
    // would kill the child and finish the run as cancelled).
    assert_eq!(reg.pause("sa_1"), ControlOutcome::Done);
    assert!(reg.cancel("sa_1"));
    assert!(token.is_cancelled());
}

#[test]
fn pause_refuses_terminal_and_unowned_runs() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let reg = ChildRegistry::new(Arc::clone(&db));
    let store = RunStore::new(&db);
    let now = hatch_core::time::now();
    store.create("sa_done", "session:s", "s", 1, "explore", "已完成", RunStatus::Completed, &now).unwrap();
    assert!(reg.register("sa_done", entry("s", "session:s")));
    assert_eq!(reg.pause("sa_done"), ControlOutcome::Terminal(RunStatus::Completed));
    assert_eq!(reg.resume("sa_done"), ControlOutcome::Terminal(RunStatus::Completed));
    assert_eq!(reg.pause("sa_nope"), ControlOutcome::NotOwned);
    assert_eq!(reg.resume("sa_nope"), ControlOutcome::NotOwned);
    assert!(!reg.is_paused("sa_nope"));
}

#[test]
fn reconcile_leaves_a_live_pause_alone() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let store = RunStore::new(&db);
    let now = hatch_core::time::now();
    // Paused, owned by this process (in the registry): untouched.
    let (s1, pid1) = LiveSleeper::spawn();
    let _g1 = s1;
    seed_running(&db, "sa_owned", pid1);
    store.pause("sa_owned").unwrap();
    let reg = ChildRegistry::new(Arc::clone(&db));
    assert!(reg.register("sa_owned", entry("s", "session:s")));
    // Paused, owned by another live process: the child is alive and
    // its owner (this test process, which spawned it) is alive.
    let (s2, pid2) = LiveSleeper::spawn();
    let _g2 = s2;
    seed_running(&db, "sa_foreign", pid2);
    store.pause("sa_foreign").unwrap();
    assert_eq!(reconcile_runs(&db, Some(&reg), "session:s", &now), 0);
    assert_eq!(store.get("sa_owned").unwrap().unwrap().status, RunStatus::Paused);
    assert_eq!(store.get("sa_foreign").unwrap().unwrap().status, RunStatus::Paused);
    assert!(store.pending_handoffs("session:s").unwrap().is_empty());
}

#[test]
fn reconcile_interrupts_a_paused_run_whose_owner_is_gone() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let store = RunStore::new(&db);
    let now = hatch_core::time::now();
    // The background job is a shell that redirects its own fds to
    // /dev/null and then stops *itself*: the stop is deterministic
    // (no `kill -STOP $!` race — stopping a freshly forked child can
    // land before it execs, leaving a stopped `sh` that still holds
    // the `output()` pipes open, so `output()` waits for an EOF that
    // never comes). The outer shell exits after 0.2s, so the stopped
    // job is reparented to pid 1: no live owner can ever resume it.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("sh -c 'exec >/dev/null 2>&1; kill -STOP $$; sleep 60' & pid=$!; echo $pid; sleep 0.2")
        .output()
        .expect("sh must exist for this test");
    let pid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().expect("the helper prints the sleeper pid");
    assert!(hatch_scheduler::lock::pid_alive(pid), "the orphaned sleeper is still alive");
    // A scratch dir for the run, as a real spawn would have left:
    // reconciliation removes it with the dead owner's mess.
    let scratch = std::env::temp_dir().join("hatch-sa-sa_orphan");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(scratch.join("probe"), "state").unwrap();
    seed_running(&db, "sa_orphan", pid as u32);
    store.pause("sa_orphan").unwrap();
    let reg = ChildRegistry::new(Arc::clone(&db));
    assert_eq!(reconcile_runs(&db, Some(&reg), "session:s", &now), 1);
    let rec = store.get("sa_orphan").unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Interrupted);
    assert!(rec.error.contains("暂停状态"), "{}", rec.error);
    // SIGKILL leaves a zombie until pid 1 reaps it, and a zombie
    // still answers a signal-0 check — poll for it to disappear.
    let mut gone = false;
    for _ in 0..250 {
        if !hatch_scheduler::lock::pid_alive(pid) {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(gone, "the stopped orphan is killed");
    assert!(!scratch.exists(), "its scratch dir is removed");
    let pending = store.pending_handoffs("session:s").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].run_id, "sa_orphan");
    assert!(pending[0].handoff.contains("暂停状态"));
    // A second reconcile is a no-op: the run is terminal now.
    assert_eq!(reconcile_runs(&db, Some(&reg), "session:s", &now), 0);
}
#[test]
fn pause_and_resume_tools_freeze_and_continue_the_same_run() {
    let e = env();
    let now = hatch_core::time::now();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let parent_ref = format!("session:{s}");
    let store = RunStore::new(&e.db);
    // A running child owned by this process, with a real pid to stop.
    let (sleeper, pid) = LiveSleeper::spawn();
    let _guard = sleeper;
    store.create("sa_1", &parent_ref, &s, 1, "explore", "查预算", RunStatus::Running, &now).unwrap();
    store.set_child_pid("sa_1", pid).unwrap();
    let token = CancelToken::new();
    assert!(registry.register(
        "sa_1",
        RegistryEntry { cancel: token.clone(), session: s.clone(), parent_ref: parent_ref.clone() }
    ));
    store.create("sa_done", &parent_ref, &s, 1, "explore", "已做完", RunStatus::Completed, &now).unwrap();
    store.create("sa_other", "session:other", "other", 1, "explore", "别人的", RunStatus::Running, &now).unwrap();
    store.create("sa_ghost", &parent_ref, &s, 1, "explore", "不在本进程", RunStatus::Running, &now).unwrap();
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
    let pause = |id: &str| {
        hatch_agent::subagent::handle_pause_call(
            &agent,
            &call("c1", "pause_subagent", json!({"run_id": id})),
            &parent_ref,
        )
    };
    let resume = |id: &str| {
        hatch_agent::subagent::handle_resume_call(
            &agent,
            &call("c1", "resume_subagent", json!({"run_id": id})),
            &parent_ref,
        )
    };
    // The async tool list offers pause and resume next to cancel.
    let mut d = Delegation::root(Arc::new(TypeRegistry::builtin()));
    d.async_mode = true;
    d.registry = Some(Arc::new(ChildRegistry::new(e.db.clone())));
    let names: Vec<String> =
        hatch_agent::subagent::tool_specs(&d).into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["delegate", "delegate_parallel", "cancel_subagent", "pause_subagent", "resume_subagent"]);
    // Pause freezes the process; the same run id, the same process.
    let h = pause("sa_1");
    assert!(h.content.contains("已暂停子代理 sa_1"), "{}", h.content);
    assert_eq!(h.runs, vec!["sa_1"]);
    assert!(wait_state(pid, "T"), "a paused process reports state T");
    assert_eq!(store.get("sa_1").unwrap().unwrap().status, RunStatus::Paused);
    let h = pause("sa_1");
    assert!(h.content.contains("已经处于暂停状态"), "{}", h.content);
    let h = resume("sa_1");
    assert!(h.content.contains("已恢复子代理 sa_1"), "{}", h.content);
    assert!(h.content.contains("从暂停处继续"), "{}", h.content);
    assert!(wait_not_state(pid, "T"), "a resumed process leaves state T");
    assert_eq!(store.get("sa_1").unwrap().unwrap().status, RunStatus::Running);
    let h = resume("sa_1");
    assert!(h.content.contains("正在运行，无需恢复"), "{}", h.content);
    // Pause then cancel: a paused run can still be cancelled, and the
    // cancel token fires so the monitor thread kills the child.
    let h = pause("sa_1");
    assert!(h.content.contains("已暂停子代理 sa_1"), "{}", h.content);
    let h = hatch_agent::subagent::handle_cancel_call(
        &agent,
        &call("c1", "cancel_subagent", json!({"run_id": "sa_1"})),
        &parent_ref,
    );
    assert!(h.content.contains("已取消子代理 sa_1"), "{}", h.content);
    assert!(token.is_cancelled());
    // The same refusals as cancel: unknown, finished, foreign, ghost.
    let h = pause("sa_nope");
    assert!(h.content.contains("没有子代理运行"), "{}", h.content);
    let h = resume("sa_nope");
    assert!(h.content.contains("没有子代理运行"), "{}", h.content);
    let h = pause("sa_done");
    assert!(h.content.contains("已经结束"), "{}", h.content);
    let h = resume("sa_done");
    assert!(h.content.contains("已经结束"), "{}", h.content);
    let h = pause("sa_other");
    assert!(h.content.contains("不属于本次对话"), "{}", h.content);
    let h = resume("sa_ghost");
    assert!(h.content.contains("不在本进程运行"), "{}", h.content);
    assert!(h.content.contains("hatch agents resume"), "{}", h.content);
    let h = hatch_agent::subagent::handle_pause_call(&agent, &call("c1", "pause_subagent", json!({})), &parent_ref);
    assert!(h.content.contains("ERROR"), "{}", h.content);
}

