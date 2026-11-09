//! D5 pause/resume with real child processes: pausing freezes the
//! child mid-sleep (its scratch dir and database are kept, the run
//! stays paused and still holds its slot), and resuming continues
//! the same run id and the same process to completion — the work is
//! continued, not restarted.

#[path = "subagent_process/common.rs"]
mod common;
use common::*;

use hatch_agent::{ChildRegistry, RunOptions, RunStatus, RunStore, SessionStore, StopReason};
use hatch_llm::ChatResponse;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn pause_freezes_a_real_child_and_resume_continues_the_same_run() {
    let e = env();
    let script = write_script(
        e._dir.path(),
        "slow.json",
        json!([{"sleep_secs": 8}, {"text": "子结论：睡醒了。"}]),
    );
    let _guard = ScriptGuard::set(&script);
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let now = hatch_core::time::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (router, seen) = model_seen(move |_| match c.fetch_add(1, Ordering::SeqCst) {
        0 => ChatResponse::calls(vec![call(
            "p1",
            "delegate",
            json!({"task": "慢慢来", "type": "explore"}),
        )]),
        1 => ChatResponse::text("知道了。"),
        2 => ChatResponse::text("收到。"),
        _ => ChatResponse::text("确认。"),
    });
    let a = agent(&e, &router, &registry, 60);
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = a.run(&s, "慢慢来", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let run_id = r.subagents[0].clone();
    let pid = RunStore::new(&e.db).get(&run_id).unwrap().unwrap().child_pid;
    assert!(pid > 0);
    assert_eq!(RunStore::new(&e.db).get(&run_id).unwrap().unwrap().status, RunStatus::Running);
    // Pause mid-sleep: the child process freezes, the run is paused,
    // and the scratch dir (its database) is kept.
    assert_eq!(registry.pause(&run_id), hatch_agent::ControlOutcome::Done);
    assert_eq!(RunStore::new(&e.db).get(&run_id).unwrap().unwrap().status, RunStatus::Paused);
    assert!(wait_state(pid, "T"), "a paused process reports state T");
    assert!(std::env::temp_dir().join(format!("hatch-sa-{run_id}")).exists());
    assert!(registry.contains(&run_id), "a paused run still holds its slot");
    // Time passes while paused, but the run does not move: the child
    // would have finished its 8s sleep by now if it had kept running.
    // A turn in between reconciles and leaves the live pause alone,
    // and no handoff is injected yet.
    std::thread::sleep(Duration::from_secs(3));
    let r = a.run(&s, "还有别的事", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(!sys(&seen, 2).contains("子结论"), "child is not done yet");
    assert_eq!(RunStore::new(&e.db).get(&run_id).unwrap().unwrap().status, RunStatus::Paused);
    assert!(wait_state(pid, "T"), "the pause still holds after a turn in between");
    // Resume: the same run id, the same process, the same sleep — the
    // child wakes and finishes from where the pause froze it.
    assert_eq!(registry.resume(&run_id), hatch_agent::ControlOutcome::Done);
    assert_eq!(RunStore::new(&e.db).get(&run_id).unwrap().unwrap().status, RunStatus::Running);
    assert!(wait_not_state(pid, "T"), "a resumed process leaves state T");
    assert_eq!(wait_terminal(&e.db, &run_id, Duration::from_secs(20)), RunStatus::Completed);
    assert!(!registry.contains(&run_id), "monitor unregistered the run");
    // The conclusion comes back as a handoff next turn, under the same
    // run id — the work was continued, not restarted.
    let r = a.run(&s, "怎么样了", &now, RunOptions::default()).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(sys(&seen, 3).contains("子结论：睡醒了"), "{}", sys(&seen, 3));
    assert!(!std::env::temp_dir().join(format!("hatch-sa-{run_id}")).exists());
    assert!(no_subagent_process());
}

