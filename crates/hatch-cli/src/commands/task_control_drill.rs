//! Control-plane drills for M1 (task 2/3 close-out + §10.2 cancel):
//! budget stops keep completed steps visible and resume after a
//! raise, and cancel stops the worker and invalidates its approvals.
//! All scripted; no model, no sends. The `--background` spawn is
//! covered by the real-process test in `tests/background_task_run.rs`
//! (`CARGO_BIN_EXE_hatch` is only available to integration tests).

#![cfg(test)]

use super::task_control::{apply_command, cancel_task};
use super::task_worker::{run_worker, KillPoint, WorkerOpts, WorkerReport};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_eval::World;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{EventLog, ExecStatus, TaskCommands, TaskStatus, TaskStore, UnitStore};
use std::path::PathBuf;
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

fn opts(task_id: Option<String>, now: Timestamp, max_model_calls: Option<u64>) -> WorkerOpts {
    WorkerOpts {
        task_id,
        kind: None,
        lease_ttl_secs: 1,
        now_override: Some(now),
        kill: KillPoint::None,
        router: None,
        max_model_calls,
    }
}

fn event_kinds(db: &Db, task_id: &str) -> Vec<(String, String)> {
    EventLog::new(db)
        .events_since(task_id, 0, 1000)
        .unwrap()
        .into_iter()
        .map(|e| (e.kind, e.detail))
        .collect()
}

/// Run a fresh brief task until the artifact step blocks on approval.
fn run_blocked(db: &Arc<Db>, dir: &PathBuf, out: &TempDir, now: Timestamp) -> WorkerReport {
    let report = run_worker(db, dir, out.path(), opts(None, now, None)).unwrap();
    assert!(report.blocked.is_some(), "第一轮应在审批上暂停");
    assert_eq!(
        report.steps_done, 2,
        "triage + gather 已完成，artifact 待批"
    );
    report
}

fn approve_write(db: &Db, id: &str, now: Timestamp) {
    let q = ApprovalQueue::new(&db);
    assert_eq!(
        q.decide(id, true, "drill approve", &now).unwrap(),
        ApprovalStatus::Approved
    );
    let exec = hatch_tools::ExecutorRegistry::new().with(Arc::new(hatch_tools::WriteFileExecutor));
    let outcome = hatch_tools::execute_approved(db, &exec, id, &now).unwrap();
    assert!(
        matches!(outcome, hatch_tools::ExecOutcome::Executed(_)),
        "审批执行应成功，实际 {outcome:?}"
    );
}

#[test]
fn budget_stop_keeps_completed_steps_visible_then_resumes() {
    let (db, dir, out, now) = setup();
    // A budget of 1 call: triage runs (its calls land in one step),
    // then the worker stops at the gather boundary instead of
    // running on — the completed step stays readable.
    let first = run_worker(&db, &dir, out.path(), opts(None, now, Some(1))).unwrap();
    assert!(first.budget_stopped, "预算耗尽时应暂停");
    assert_eq!(first.steps_done, 1, "triage 已完成，gather 不应开始");
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Waiting);
    assert_eq!(task.max_model_calls, 1);
    let units = UnitStore::new(&db);
    assert_eq!(
        units
            .get_step(&first.task_id, "triage")
            .unwrap()
            .unwrap()
            .status,
        ExecStatus::Done,
        "超预算后已完成步骤仍可查（task show / 事件）"
    );
    assert!(
        event_kinds(&db, &first.task_id)
            .iter()
            .any(|(k, _)| k == "budget_exceeded"),
        "超预算事件应写入 task_events"
    );
    // Raise the budget: the task resumes from gather, then blocks on
    // the artifact approval as usual, and finishes after approval.
    TaskStore::new(&db)
        .set_budget(&first.task_id, 100, &now)
        .unwrap();
    let second = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now, None),
    )
    .unwrap();
    assert!(!second.budget_stopped, "提额后不应再因预算暂停");
    assert!(second.blocked.is_some(), "第二轮应在审批上暂停");
    assert_eq!(second.steps_done, 2, "gather 已完成");
    let approval_id = units
        .get_step(&first.task_id, "artifact")
        .unwrap()
        .unwrap()
        .approval_id;
    approve_write(&db, &approval_id, now);
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now, None),
    )
    .unwrap();
    assert_eq!(report.steps_done, 3);
    assert!(report.blocked.is_none());
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    // The report is per-run (the final run calls no model); the
    // cumulative total lives in the ledger and keeps growing.
    assert!(
        units.total_model_calls(&first.task_id).unwrap() > 1,
        "提额后模型调用应继续累计"
    );
}

#[test]
fn cancel_stops_worker_and_invalidates_approval() {
    let (db, dir, out, now) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let approval_id = UnitStore::new(&db)
        .get_step(&first.task_id, "artifact")
        .unwrap()
        .unwrap()
        .approval_id;
    // Cancel while blocked: the task stops and its pending approval dies.
    let msg = cancel_task(&db, &first.task_id, "测试取消", &now).unwrap();
    assert!(msg.contains("已取消"), "取消应说明结果，实际 {msg}");
    assert!(
        msg.contains("1 个审批已失效"),
        "取消应说明失效数，实际 {msg}"
    );
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert_eq!(task.error, "测试取消");
    assert_eq!(
        ApprovalQueue::new(&db).get(&approval_id).unwrap().status,
        ApprovalStatus::Invalidated
    );
    assert!(
        event_kinds(&db, &first.task_id)
            .iter()
            .any(|(k, _)| k == "task_cancelled"),
        "取消事件应写入 task_events"
    );
    // A late approval can no longer trigger an action.
    let q = ApprovalQueue::new(&db);
    assert!(
        q.decide(&approval_id, true, "late decide", &now).is_err(),
        "取消后审批不得再决定"
    );
    // The worker refuses to resume a cancelled task.
    let result = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now, None),
    );
    assert!(result.is_err(), "已取消的任务不得继续执行");
    assert!(!out.path().join("brief-v1.md").exists(), "取消后不得写文件");
}

/// A mid-task command (§10.2) moves the plan version and invalidates
/// the old version's approvals; a stale command is rejected.
#[test]
fn version_bump_invalidates_old_approval() {
    let (db, dir, out, now) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let id = UnitStore::new(&db)
        .get_step(&first.task_id, "artifact")
        .unwrap()
        .unwrap()
        .approval_id;
    // A mid-task command moves the plan v1 → v2; the old approval dies.
    let msg = apply_command(&db, &first.task_id, 1, "改收件人：换成王宇", "drill", &now).unwrap();
    assert!(msg.contains("v1 → v2"), "指令应说明版本移动，实际 {msg}");
    assert!(
        msg.contains("1 个旧审批已失效"),
        "指令应说明失效数，实际 {msg}"
    );
    assert_eq!(
        ApprovalQueue::new(&db).get(&id).unwrap().status,
        ApprovalStatus::Invalidated
    );
    let q = ApprovalQueue::new(&db);
    assert!(
        q.decide(&id, true, "late decide", &now).is_err(),
        "失效的审批不得再决定"
    );
    let exec = hatch_tools::ExecutorRegistry::new().with(Arc::new(hatch_tools::WriteFileExecutor));
    assert!(
        hatch_tools::execute_approved(&db, &exec, &id, &now).is_err(),
        "失效的审批不得再执行"
    );
    // The worker's resume hits the invalidated approval and fails the
    // task visibly; the file is never written.
    let result = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now, None),
    );
    assert!(result.is_err(), "失效审批不得触发新动作");
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(
        task.error.contains("invalidated"),
        "失败原因须说明审批已随版本失效，实际 {}",
        task.error
    );
    assert!(
        !out.path().join("brief-v1.md").exists(),
        "失效审批不得写文件"
    );
    assert!(
        event_kinds(&db, &first.task_id)
            .iter()
            .any(|(k, d)| k == "plan_changed" && d.contains("1 个旧审批已失效")),
        "版本变更事件应记录失效数"
    );
    // A stale command (still expecting v1) is rejected as expired and
    // recorded as rejected.
    let err = apply_command(&db, &first.task_id, 1, "再改一次", "drill", &now).unwrap_err();
    assert!(
        err.to_string().contains("版本已推进，指令过期"),
        "过期指令应说明原因，实际 {err}"
    );
    let commands = TaskCommands::new(&db).for_task(&first.task_id).unwrap();
    assert_eq!(
        commands.last().map(|c| c.status.as_str()),
        Some("rejected"),
        "过期指令应记录为 rejected"
    );
    assert!(
        event_kinds(&db, &first.task_id)
            .iter()
            .any(|(k, d)| k == "command_rejected" && d.contains("指令已拒绝")),
        "过期指令应有拒绝事件"
    );
}
