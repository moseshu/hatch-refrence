//! Approval fault-injection drills for the M1 worker (task 4):
//! approve twice, approve-then-crash, orphaned approvals, external
//! writes, expired approvals, and version-bump invalidation. Every
//! drill runs on the scripted meeting-brief world; sends are never
//! real (the brief only writes a file, behind an approval).

#![cfg(test)]

use super::task_worker::{run_worker, KillPoint, WorkerOpts, WorkerReport};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_eval::World;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::{params, Db};
use hatch_tasks::{EventLog, TaskStatus, TaskStore, UnitStore};
use hatch_tools::ws_exec::WritePayload;
use hatch_tools::{execute_approved, ExecOutcome, ExecutorRegistry, WriteFileExecutor};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief")
}

fn setup() -> (Arc<Db>, PathBuf, TempDir, Timestamp) {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let dir = world_dir();
    let world = World::load(&dir).unwrap();
    let now = parse_rfc3339(&world.persona.now).unwrap();
    (db, dir, TempDir::new().unwrap(), now)
}

fn opts(task_id: Option<String>, now: Timestamp) -> WorkerOpts {
    WorkerOpts {
        task_id,
        kind: None,
        lease_ttl_secs: 1,
        now_override: Some(now),
        kill: KillPoint::None,
        router: None,
        max_model_calls: None,
    }
}

fn artifact_approval_id(db: &Db, task_id: &str) -> String {
    UnitStore::new(db)
        .get_step(task_id, "artifact")
        .unwrap()
        .unwrap()
        .approval_id
}

/// How many `write_file` approvals exist for one origin (a second
/// dispatch would show up here as a second row).
fn count_write_approvals(db: &Db, origin: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM approvals WHERE origin = ?1 AND kind = 'write_file'",
            params![origin],
            |r| r.get(0),
        )
        .unwrap()
}

/// Run a fresh brief task: triage + gather run, then the artifact
/// step blocks on its `fs_write` approval.
fn run_blocked(db: &Arc<Db>, dir: &Path, out: &TempDir, now: Timestamp) -> WorkerReport {
    let report = run_worker(db, dir, out.path(), opts(None, now)).unwrap();
    assert!(report.blocked.is_some(), "第一轮应在审批上暂停");
    assert_eq!(
        report.steps_done, 2,
        "triage + gather 已完成，artifact 待批"
    );
    assert!(!out.path().join("brief-v1.md").exists(), "批准前不得写文件");
    report
}

/// Decide an approval and execute it (the `hatch approvals approve`
/// path, in-process). Returns the execution outcome.
fn approve_write(db: &Db, id: &str, now: Timestamp) -> ExecOutcome {
    let q = ApprovalQueue::new(db);
    let status = q.decide(id, true, "drill approve", &now).unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(WriteFileExecutor));
    let outcome = execute_approved(db, &exec, id, &now).unwrap();
    assert!(
        matches!(outcome, ExecOutcome::Executed(_)),
        "审批执行应成功，实际 {outcome:?}"
    );
    outcome
}

fn step_done_count(db: &Db, task_id: &str, step_id: &str) -> usize {
    EventLog::new(db)
        .events_since(task_id, 0, 1000)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "step_done" && e.step_id == step_id)
        .count()
}

#[test]
fn duplicate_approval_executes_once() {
    let (db, dir, out, now) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let id = artifact_approval_id(&db, &first.task_id);
    // Decide twice: the second decision is refused.
    let q = ApprovalQueue::new(&db);
    assert_eq!(
        q.decide(&id, true, "first", &now).unwrap(),
        ApprovalStatus::Approved
    );
    assert!(
        q.decide(&id, true, "second", &now).is_err(),
        "已决定的审批不得再决定"
    );
    // Execute twice: the second execution is refused, not re-run.
    let exec = ExecutorRegistry::new().with(Arc::new(WriteFileExecutor));
    let outcome = execute_approved(&db, &exec, &id, &now).unwrap();
    assert!(matches!(outcome, ExecOutcome::Executed(_)));
    assert!(
        execute_approved(&db, &exec, &id, &now).is_err(),
        "已执行的审批不得再执行"
    );
    // Resume: the worker completes from the breakpoint — one file,
    // one step_done, and the artifact step carries its receipt.
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    )
    .unwrap();
    assert_eq!(report.steps_done, 3);
    assert!(report.blocked.is_none());
    assert!(out.path().join("brief-v1.md").exists());
    assert_eq!(step_done_count(&db, &first.task_id, "artifact"), 1);
    let step = UnitStore::new(&db)
        .get_step(&first.task_id, "artifact")
        .unwrap()
        .unwrap();
    assert!(!step.receipt_id.is_empty(), "artifact 步骤应记录回执");
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
}

#[test]
fn approve_then_crash_resumes_from_ledger() {
    let (db, dir, out, now) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let id = artifact_approval_id(&db, &first.task_id);
    // The human approves and the write executes while the worker is
    // dead; the worker then resumes from the ledger (not from memory).
    approve_write(&db, &id, now);
    let path = out.path().join("brief-v1.md");
    let before = std::fs::read_to_string(&path).unwrap();
    let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    )
    .unwrap();
    assert_eq!(report.steps_done, 3);
    assert!(report.blocked.is_none());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "恢复不得重写简报"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        mtime_before,
        "恢复不得重写简报"
    );
    assert_eq!(
        count_write_approvals(&db, &format!("task:{}:artifact", first.task_id)),
        1,
        "恢复不得再排一个审批"
    );
    assert_eq!(step_done_count(&db, &first.task_id, "artifact"), 1);
}

#[test]
fn orphan_approval_is_adopted_not_recreated() {
    let (db, dir, out, now) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let id = artifact_approval_id(&db, &first.task_id);
    // Simulate the crash between dispatch and record: the approval
    // exists, but the step never recorded its id.
    db.conn()
        .execute(
            "UPDATE task_steps SET approval_id = '' WHERE task_id = ?1 AND step_id = 'artifact'",
            params![first.task_id],
        )
        .unwrap();
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    )
    .unwrap();
    assert!(report.blocked.is_some(), "孤儿审批仍待批，任务应继续暂停");
    assert_eq!(report.steps_done, 2);
    assert_eq!(
        artifact_approval_id(&db, &first.task_id),
        id,
        "应复用同一个审批"
    );
    assert_eq!(
        count_write_approvals(&db, &format!("task:{}:artifact", first.task_id)),
        1,
        "不得再排一个审批"
    );
    // The normal path still finishes from the adopted approval.
    approve_write(&db, &id, now);
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    )
    .unwrap();
    assert_eq!(report.steps_done, 3);
    assert!(report.blocked.is_none());
}

#[test]
fn external_write_after_execute_makes_failure_visible() {
    let (db, dir, out, now) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let id = artifact_approval_id(&db, &first.task_id);
    approve_write(&db, &id, now);
    // An external actor overwrites the brief after the approval
    // executed: the worker must fail visibly, never silently rewrite.
    let path = out.path().join("brief-v1.md");
    std::fs::write(&path, "外部篡改的简报\n").unwrap();
    let result = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    );
    assert!(result.is_err(), "外部篡改后恢复应失败而非悄悄重写");
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(
        task.error.contains("内容与 gather 结果不一致"),
        "失败原因须说明外部修改，实际 {}",
        task.error
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "外部篡改的简报\n",
        "篡改的文件应原样保留，供人工核查"
    );
    assert_eq!(
        count_write_approvals(&db, &format!("task:{}:artifact", first.task_id)),
        1,
        "不得自动重复提交"
    );
}

#[test]
fn external_write_cannot_bypass_approval() {
    let (db, dir, out, now) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let id = artifact_approval_id(&db, &first.task_id);
    // An external actor writes the exact expected content while the
    // approval is still pending: the worker must still wait for the
    // human — an external write never bypasses the approval gate.
    let approval = ApprovalQueue::new(&db).get(&id).unwrap();
    let payload: WritePayload = serde_json::from_value(approval.payload.clone()).unwrap();
    std::fs::write(out.path().join(&payload.path), &payload.content).unwrap();
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    )
    .unwrap();
    assert!(report.blocked.is_some(), "审批未决前外部写入不得让步骤完成");
    assert_eq!(report.steps_done, 2, "artifact 步骤仍未完成");
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    // The executor's checkpoint catches the external modification:
    // the approved write is refused, and the task then fails visibly
    // instead of silently overwriting the external file.
    let exec = ExecutorRegistry::new().with(Arc::new(WriteFileExecutor));
    let q = ApprovalQueue::new(&db);
    assert_eq!(
        q.decide(&id, true, "drill approve", &now).unwrap(),
        ApprovalStatus::Approved
    );
    let outcome = execute_approved(&db, &exec, &id, &now).unwrap();
    assert!(
        matches!(&outcome, ExecOutcome::Failed(msg) if msg.contains("在审批之后被修改过")),
        "外部写入后执行应被检查点拒绝，实际 {outcome:?}"
    );
    let result = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    );
    assert!(result.is_err(), "审批执行失败后恢复应失败而非自动重试");
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(
        task.error.contains("已failed"),
        "失败原因须说明审批已失败，实际 {}",
        task.error
    );
}

#[test]
fn expired_approval_never_executes() {
    let (db, dir, out, now) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let id = artifact_approval_id(&db, &first.task_id);
    // The fs_write approval lives 24h; deciding 25h later expires it.
    let later = now + chrono::Duration::hours(25);
    let q = ApprovalQueue::new(&db);
    assert_eq!(
        q.decide(&id, true, "late approve", &later).unwrap(),
        ApprovalStatus::Expired,
        "过期的审批应标记 expired，而非 approved"
    );
    assert_eq!(q.get(&id).unwrap().status, ApprovalStatus::Expired);
    let result = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), later),
    );
    assert!(result.is_err(), "过期审批不得触发新动作");
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(
        task.error.contains("expired"),
        "失败原因须说明审批已过期，实际 {}",
        task.error
    );
    assert!(
        !out.path().join("brief-v1.md").exists(),
        "过期审批不得写文件"
    );
}
