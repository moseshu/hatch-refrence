//! The task 7 comparison (acceptance (b)): the same batch of three
//! meeting briefs, run once as delegated-parallel children joined by
//! a parent, and once as the same three briefs one after another by
//! a single worker. Script model only, fixture world — no live model,
//! no real mail. Reports completion rate, wall time and overhead
//! (model calls + extra ledger rows).
//!
//! The brief's artifact step pauses on an approval, so each child is
//! driven the way the M1 E2E drives it: run, approve the artifact
//! approval in-process, run again to completion. The approval is
//! executed with the same `WriteFileExecutor` the CLI uses.

#![cfg(test)]

use super::task_worker::{run_worker, WorkerOpts};
use hatch_core::time::Timestamp;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{ChildSpec, DelegationStore, TaskApprovals, TaskStatus, TaskStore};
use hatch_tools::{execute_approved, ExecutorRegistry, WriteFileExecutor};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

fn now() -> Timestamp {
    hatch_core::time::now()
}

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief")
}

/// A private copy of the world for one brief: parallel children
/// must not share the triage's classification writes (§6.1: shared
/// read-only input, own workspaces). Without this, each child
/// re-classifies the same mails and the comparison measures a
/// shared-mailbox race, not orchestration overhead.
fn copy_world(src: &PathBuf, dst: &PathBuf) -> PathBuf {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        std::fs::copy(entry.path(), dst.join(name)).unwrap();
    }
    dst.clone()
}

/// Run one brief task to completion (script model): run, approve the
/// artifact approval, run again. Returns the total model calls.
fn run_brief_to_done(db: &Arc<Db>, world: &Path, out: &Path, task_id: &str) -> u64 {
    let mut calls = 0;
    for _ in 0..2 {
        let opts = WorkerOpts {
            task_id: Some(task_id.to_string()),
            ..Default::default()
        };
        let report = run_worker(db, world, out, opts).unwrap();
        calls += report.model_calls;
        let status = TaskStore::new(db).get(task_id).unwrap().unwrap().status;
        if status == TaskStatus::Done {
            break;
        }
        approve_pending_artifact(db, task_id);
    }
    let status = TaskStore::new(db).get(task_id).unwrap().unwrap().status;
    assert_eq!(status, TaskStatus::Done, "简报任务 {task_id} 应完成");
    calls
}

/// Approve the task's pending artifact approval in-process (the CLI
/// `approvals approve` path, minus the CLI: decide + execute with the
/// file executor).
fn approve_pending_artifact(db: &Db, task_id: &str) {
    let linked = TaskApprovals::new(db).for_task(task_id).unwrap();
    let pending = linked.iter().find_map(|a| {
        let entry = ApprovalQueue::new(db).get(&a.approval_id).ok()?;
        (entry.status == ApprovalStatus::Pending).then_some(a.approval_id.clone())
    });
    let id = pending.expect("简报任务应有待审批的写文件审批");
    let status = ApprovalQueue::new(db)
        .decide(&id, true, "comparison drill approve", &now())
        .unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(WriteFileExecutor));
    let outcome = execute_approved(db, &exec, &id, &now()).unwrap();
    assert!(
        matches!(
            outcome,
            hatch_tools::ExecOutcome::Executed(_) | hatch_tools::ExecOutcome::AlreadyDone(_)
        ),
        "审批执行应成功：{outcome:?}"
    );
}

fn ledger_rows(db: &Db) -> (i64, i64) {
    let tasks: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
        .unwrap();
    let events: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM task_events", [], |r| r.get(0))
        .unwrap();
    (tasks, events)
}

#[test]
fn delegated_parallel_vs_single_agent_comparison() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("hatch.db")).unwrap());
    let world = world_dir();

    // Shared setup (not measured): classify the fixture mailbox once,
    // so the triage's classification writes — shared state in the Db,
    // not per-brief work — are not duplicated by the parallel children
    // or counted twice. Both paths below then do the same gather work.
    {
        let setup = TaskStore::new(&db)
            .create(
                "meeting-brief",
                "对照前置：分类",
                "drill",
                "",
                3,
                None,
                &now(),
            )
            .unwrap();
        run_brief_to_done(&db, &world, &dir.path().join("out-setup"), &setup.id);
    }

    // --- Delegated: three children in parallel threads + a join. ---
    let delegated_start = Instant::now();
    let parent = TaskStore::new(&db)
        .create(
            "delegate-join",
            "对照：并行委派",
            "drill",
            "",
            3,
            None,
            &now(),
        )
        .unwrap();
    let delegations = DelegationStore::new(&db);
    let mut children = Vec::new();
    for (i, step) in ["r1", "r2", "r3"].iter().enumerate() {
        let d = delegations
            .spawn(
                &parent.id,
                step,
                ChildSpec::new("meeting-brief", &format!("简报分支 {i}"), "drill", 0),
                &now(),
            )
            .unwrap();
        children.push(d.child_task);
    }
    let before = ledger_rows(&db);
    let handles: Vec<_> = children
        .iter()
        .enumerate()
        .map(|(i, child)| {
            let db = Arc::clone(&db);
            let world = copy_world(&world, &dir.path().join(format!("world-d{i}")));
            let out = dir.path().join(format!("out-d{i}"));
            let child = child.clone();
            std::thread::spawn(move || run_brief_to_done(&db, &world, &out, &child))
        })
        .collect();
    let mut delegated_calls = 0u64;
    for h in handles {
        delegated_calls += h.join().unwrap();
    }
    let opts = WorkerOpts {
        task_id: Some(parent.id.clone()),
        ..Default::default()
    };
    let join_report = run_worker(&db, &world, &dir.path().join("out-join"), opts).unwrap();
    delegated_calls += join_report.model_calls;
    let delegated_elapsed = delegated_start.elapsed();
    let parent_status = TaskStore::new(&db).get(&parent.id).unwrap().unwrap().status;
    assert_eq!(parent_status, TaskStatus::Done, "父任务应在汇合后完成");
    let after_delegated = ledger_rows(&db);

    // --- Single: the same three briefs, one worker after another. ---
    let single_start = Instant::now();
    let mut single_calls = 0u64;
    for i in 0..3 {
        let world = copy_world(&world, &dir.path().join(format!("world-s{i}")));
        let out = dir.path().join(format!("out-s{i}"));
        single_calls += run_brief_to_done(&db, &world, &out, &{
            let t = TaskStore::new(&db)
                .create(
                    "meeting-brief",
                    &format!("简报单跑 {i}"),
                    "drill",
                    "",
                    3,
                    None,
                    &now(),
                )
                .unwrap();
            t.id
        });
    }
    let single_elapsed = single_start.elapsed();
    let after_single = ledger_rows(&db);

    // --- The three numbers. ---
    let delegated_done = 3;
    let single_done = 3;
    eprintln!("对照评估（脚本模型，3 份会前简报）：");
    eprintln!("  完成率：并行委派 {delegated_done}/3，单代理 {single_done}/3");
    eprintln!(
        "  耗时：并行委派 {:?}，单代理 {:?}",
        delegated_elapsed, single_elapsed
    );
    eprintln!(
        "  开销：模型调用 并行委派 {delegated_calls} 次，单代理 {single_calls} 次；账本行 并行委派 {} 任务 / {} 事件，单代理累计 {} 任务 / {} 事件",
        after_delegated.0 - before.0,
        after_delegated.1 - before.1,
        after_single.0 - after_delegated.0,
        after_single.1 - after_delegated.1
    );
    assert_eq!(delegated_done, 3);
    assert_eq!(single_done, 3);
    assert_eq!(
        delegated_calls, single_calls,
        "同一批工作，委派不应多花模型调用（汇合是代码，不是生成）"
    );
    assert!(
        delegated_elapsed <= single_elapsed + std::time::Duration::from_secs(5),
        "并行委派不应比单跑慢太多：并行 {delegated_elapsed:?}，单跑 {single_elapsed:?}"
    );
}
