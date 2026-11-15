//! Resident-worker drills (P0 product closed loop): the daemon-side
//! worker claims queued tasks, runs them to waiting/done with no
//! `hatch task run`, resumes by step after a worker restart, wakes
//! on approval decisions and subtask completion, and honours
//! pause/resume through the control plane. Script model + the
//! fixture meeting-brief world only — no real model, no real
//! accounts. The only `run_worker` calls in this file are the ones
//! the resident worker itself makes through `sweep_once` /
//! `run_loop`; nothing here invokes `hatch task run`.

#![cfg(test)]

use crate::app::App;
use crate::commands::approval_service::{ApprovalService, ServiceOutcome};
use crate::commands::task_delegate::delegate_task;
use crate::commands::task_resident::{wake, ResidentWorker};
use crate::commands::task_run_params::RunParams;
use hatch_core::time::now;
use hatch_policy::{ActionLedger, ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{
    CreateOutcome, DelegationStatus, DelegationStore, EventLog, TaskStatus, TaskStore, UnitStore,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief")
}

/// A fresh home with the resident sweep interval set in config.toml.
/// A long interval (drill c) proves the wake path: the task can only
/// complete because the approval decision woke the worker.
fn setup(sweep_secs: u64) -> (Arc<App>, TempDir) {
    let home = TempDir::new().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("[tasks]\nresident_sweep_secs = {sweep_secs}\n"),
    )
    .unwrap();
    let app = Arc::new(App::open(Some(home.path())).unwrap());
    (app, home)
}

fn brief_params() -> RunParams {
    RunParams::from_create_body(&serde_json::json!({
        "kind": "meeting-brief",
        "title": "会前准备",
        "world": world_dir().to_string_lossy(),
        "source": "fixture",
    }))
    .unwrap()
}

/// Create a task exactly the way the API handler does (same store
/// call, same source write) — the HTTP layer is covered by the
/// api/tasks.rs tests; these drills cover the resident worker.
fn create_task(app: &Arc<App>, params: &RunParams) -> String {
    let store = TaskStore::new(&app.db);
    let hash = params.payload_hash().unwrap();
    let json = params.to_canonical_json().unwrap();
    let task = match store
        .create_with_params(
            &params.kind,
            &params.title,
            "drill",
            "",
            3,
            None,
            &hash,
            &json,
            &now(),
        )
        .unwrap()
    {
        CreateOutcome::Created(t) => t,
        other => panic!("创建应成功，实际 {other:?}"),
    };
    store.set_source(&task.id, &params.source, &now()).unwrap();
    task.id
}

fn status(db: &Db, id: &str) -> TaskStatus {
    TaskStore::new(db).get(id).unwrap().unwrap().status
}

fn wait_status(db: &Db, id: &str, want: TaskStatus, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if status(db, id) == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Sweep until the task reaches `want` (or the deadline). Returns
/// whether it got there.
fn sweep_until(
    app: &Arc<App>,
    worker: &mut ResidentWorker,
    id: &str,
    want: TaskStatus,
    timeout_ms: u64,
) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if status(&app.db, id) == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = worker.sweep_once();
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The artifact approval the worker queued for this task.
fn artifact_approval(db: &Db, task_id: &str) -> String {
    let step = UnitStore::new(db)
        .get_step(task_id, "artifact")
        .unwrap()
        .expect("artifact 步骤应已记录");
    assert!(!step.approval_id.is_empty(), "artifact 步骤应已记录审批 id");
    step.approval_id
}

fn step_done_counts(db: &Db, task_id: &str) -> std::collections::HashMap<String, usize> {
    let mut counts = std::collections::HashMap::new();
    for e in EventLog::new(db).events_since(task_id, 0, 1000).unwrap() {
        if e.kind == "step_done" {
            *counts.entry(e.step_id).or_insert(0) += 1;
        }
    }
    counts
}

fn unit_count(db: &Db, task_id: &str, step: &str) -> i64 {
    let conn = db.conn();
    conn.query_row(
        "SELECT COUNT(*) FROM task_units WHERE task_id = ?1 AND step_id = ?2",
        hatch_store::params![task_id, step],
        |r| r.get(0),
    )
    .unwrap()
}

/// (a) API-only submit → the resident worker runs the task to
/// waiting (approval) with zero `hatch task run` invocations, and
/// the brief is not written yet (the artifact approval is pending).
#[test]
fn resident_runs_api_task_to_waiting_without_cli_run() {
    let (app, _home) = setup(2);
    let id = create_task(&app, &brief_params());
    let mut worker = ResidentWorker::new(&app);
    assert!(
        sweep_until(&app, &mut worker, &id, TaskStatus::Blocked, 30_000),
        "常驻 worker 应把任务跑到等待审批"
    );
    let counts = step_done_counts(&app.db, &id);
    assert_eq!(counts.get("triage"), Some(&1), "triage 只应完成一次");
    assert_eq!(counts.get("gather"), Some(&1), "gather 只应完成一次");
    assert_eq!(counts.get("artifact"), None, "artifact 未批，不应完成");
    let out = app.home.join("task-out").join(&id).join("brief-v1.md");
    assert!(!out.exists(), "审批未通过前不应写出简报");
    assert!(!artifact_approval(&app.db, &id).is_empty());
}

/// (b) Worker restart mid-task: a fresh `ResidentWorker` (new loop
/// state, same ledger) resumes from the breakpoint — no step is
/// redone and the brief is written.
#[test]
fn resident_resumes_by_step_after_worker_restart() {
    let (app, _home) = setup(2);
    let id = create_task(&app, &brief_params());
    let mut worker1 = ResidentWorker::new(&app);
    assert!(sweep_until(
        &app,
        &mut worker1,
        &id,
        TaskStatus::Blocked,
        30_000
    ));
    // The daemon restarts: the old worker is dropped, a new one
    // starts on the same ledger. No step state lives in the worker.
    drop(worker1);
    let mut worker2 = ResidentWorker::new(&app);
    let approval = artifact_approval(&app.db, &id);
    let outcome = ApprovalService::new(&app)
        .approve_and_execute(&approval, "drill approve", false, &now())
        .unwrap();
    assert!(
        matches!(outcome, ServiceOutcome::Executed(_)),
        "审批应执行写文件，实际 {outcome:?}"
    );
    assert!(sweep_until(
        &app,
        &mut worker2,
        &id,
        TaskStatus::Done,
        30_000
    ));
    let counts = step_done_counts(&app.db, &id);
    assert_eq!(counts.get("triage"), Some(&1), "重启后 triage 不应重做");
    assert_eq!(counts.get("gather"), Some(&1), "重启后 gather 不应重做");
    assert_eq!(counts.get("artifact"), Some(&1), "artifact 应完成一次");
    assert_eq!(unit_count(&app.db, &id, "triage"), 1);
    assert_eq!(unit_count(&app.db, &id, "gather"), 1);
    assert_eq!(unit_count(&app.db, &id, "artifact"), 1);
    let out = app.home.join("task-out").join(&id).join("brief-v1.md");
    assert!(out.exists(), "简报应已写出");
    assert!(out.metadata().unwrap().len() > 0);
}

/// (c) Approval via the service + a wake completes the task even
/// though the sweep interval is 120s: the wake, not the sweep, is
/// what moves the worker (the thread runs the real `run_loop`).
#[test]
fn resident_thread_wakes_on_approval_without_sweep() {
    let (app, _home) = setup(120);
    let (stop, handle) = crate::commands::task_resident::spawn_resident_worker(&app);
    let id = create_task(&app, &brief_params());
    wake(&app); // the API create handler wakes the worker on submit
    assert!(
        wait_status(&app.db, &id, TaskStatus::Blocked, 30_000),
        "常驻线程应在唤醒后把任务跑到等待审批"
    );
    let approval = artifact_approval(&app.db, &id);
    let outcome = ApprovalService::new(&app)
        .approve_and_execute(&approval, "drill approve", false, &now())
        .unwrap();
    assert!(matches!(outcome, ServiceOutcome::Executed(_)));
    wake(&app); // the HTTP approve handler wakes the worker
    assert!(
        wait_status(&app.db, &id, TaskStatus::Done, 10_000),
        "审批通过后常驻线程应在 10s 内完成任务——120s 的 sweep 间隔内不可能靠轮询做到"
    );
    drop(stop);
    handle.join().expect("常驻线程应干净退出");
}

/// (d) Subtask completion wakes the parent: the child brief runs to
/// done through the resident worker, then the delegate-join parent
/// is claimed and consumes it — exactly once.
#[test]
fn resident_wakes_parent_on_subtask_completion() {
    let (app, _home) = setup(2);
    let parent_params = RunParams::from_create_body(&serde_json::json!({
        "kind": "delegate-join",
        "title": "并行简报汇总",
        "world": world_dir().to_string_lossy(),
        "source": "fixture",
    }))
    .unwrap();
    let parent = create_task(&app, &parent_params);
    let msg = delegate_task(&app.db, &parent, "s1", "meeting-brief", "子简报", 0, &now()).unwrap();
    let child = DelegationStore::new(&app.db)
        .list_for_parent(&parent)
        .unwrap()
        .into_iter()
        .next()
        .expect("应已派生一个子任务")
        .child_task;
    assert!(msg.contains(&child));
    // The child inherits the parent's world, so the resident worker
    // can run it with no further input.
    let child_params = RunParams::load(&app.db, &child).unwrap();
    assert!(child_params.is_some(), "子任务应继承执行参数");
    assert_eq!(child_params.unwrap().world, Some(world_dir()));

    let mut worker = ResidentWorker::new(&app);
    // The parent is not ready while its child is still out.
    assert_eq!(status(&app.db, &parent), TaskStatus::Queued);
    assert!(
        sweep_until(&app, &mut worker, &child, TaskStatus::Blocked, 30_000),
        "常驻 worker 应把子任务跑到等待审批"
    );
    assert_eq!(
        status(&app.db, &parent),
        TaskStatus::Queued,
        "子任务未完成前父任务不应被领取"
    );
    let approval = artifact_approval(&app.db, &child);
    let outcome = ApprovalService::new(&app)
        .approve_and_execute(&approval, "drill approve", false, &now())
        .unwrap();
    assert!(matches!(outcome, ServiceOutcome::Executed(_)));
    assert!(
        sweep_until(&app, &mut worker, &child, TaskStatus::Done, 30_000),
        "子任务应在审批后完成"
    );
    assert!(
        sweep_until(&app, &mut worker, &parent, TaskStatus::Done, 30_000),
        "子任务完成后父任务应被自动唤醒并汇合"
    );
    let d = DelegationStore::new(&app.db)
        .list_for_parent(&parent)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(d.status, DelegationStatus::Consumed, "子任务应被消费一次");
    let counts = step_done_counts(&app.db, &parent);
    assert_eq!(counts.get("s1"), Some(&1), "父任务的委派步骤只应完成一次");
    assert_eq!(unit_count(&app.db, &child, "triage"), 1);
    assert_eq!(unit_count(&app.db, &child, "gather"), 1);
}

/// (f) Pause via the control plane stops the worker at the boundary
/// (the sweep leaves a paused task alone); resume re-queues it and
/// the worker continues — no manual `task run`.
#[test]
fn resident_honours_pause_and_resume() {
    let (app, _home) = setup(2);
    let id = create_task(&app, &brief_params());
    crate::commands::task_control::pause_task(&app.db, &id, "drill pause", &now()).unwrap();
    assert_eq!(status(&app.db, &id), TaskStatus::Blocked);
    let mut worker = ResidentWorker::new(&app);
    assert!(!worker.sweep_once(), "已暂停的任务不应被领取");
    assert_eq!(status(&app.db, &id), TaskStatus::Blocked);
    crate::commands::task_control::resume_task(&app.db, &id, "drill resume", &now()).unwrap();
    assert_eq!(status(&app.db, &id), TaskStatus::Queued);
    assert!(
        sweep_until(&app, &mut worker, &id, TaskStatus::Blocked, 30_000),
        "恢复后常驻 worker 应把任务跑到等待审批"
    );
    let approval = artifact_approval(&app.db, &id);
    ApprovalService::new(&app)
        .approve_and_execute(&approval, "drill approve", false, &now())
        .unwrap();
    assert!(sweep_until(
        &app,
        &mut worker,
        &id,
        TaskStatus::Done,
        30_000
    ));
}

/// NeedsReview (P0 #1): a decided approval whose execution outcome
/// is unknown waits — it does not fail the task and is not retried;
/// the ledger stays `in_progress` for a human to reconcile.
#[test]
fn resident_waits_on_needs_review_instead_of_failing() {
    let (app, _home) = setup(2);
    let id = create_task(&app, &brief_params());
    let mut worker = ResidentWorker::new(&app);
    assert!(sweep_until(
        &app,
        &mut worker,
        &id,
        TaskStatus::Blocked,
        30_000
    ));
    let approval = artifact_approval(&app.db, &id);
    // Decide without executing (the P0 #1 drill's crash simulation),
    // then open the ledger entry and never close it — as if the
    // executor crashed mid-flight.
    let q = ApprovalQueue::new(&app.db);
    let a = q.get(&approval).unwrap();
    assert_eq!(
        q.decide(&approval, true, "drill decide", &now()).unwrap(),
        ApprovalStatus::Approved
    );
    let ledger = ActionLedger::new(&app.db);
    assert!(matches!(
        ledger.begin(&a.idempotency_key, &a.kind, &now()).unwrap(),
        hatch_policy::LedgerState::Fresh
    ));
    // The worker settles the decided approval, finds the outcome
    // unknown, marks it NeedsReview — and the task waits, it does
    // not fail.
    assert!(worker.sweep_once(), "常驻 worker 应尝试结算已决定的审批");
    assert_eq!(
        status(&app.db, &id),
        TaskStatus::Blocked,
        "结果不明的审批不应让任务失败"
    );
    assert_eq!(
        q.get(&approval).unwrap().status,
        ApprovalStatus::NeedsReview,
        "审批应被标记为待人工核查"
    );
    assert_eq!(
        ledger.status(&a.idempotency_key).unwrap(),
        Some("in_progress".to_string()),
        "账本应保持 in_progress，而不是失败"
    );
    // A second sweep does not spin: NeedsReview is not "decided",
    // so the task is not ready again.
    assert!(!worker.sweep_once(), "待核查的任务不应被重复领取");
    assert_eq!(status(&app.db, &id), TaskStatus::Blocked);
}
