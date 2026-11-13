//! Drills for task 5 (planning list + control plane): the list
//! projection is one read of the ledger, concurrent edits race on the
//! version, edit-step refuses executed steps, and a gated step can be
//! edited and resumed on a fresh approval. Pause, plan-step edit,
//! and event-replay drills live in `task_pause_drill` (helpers here
//! are shared). Every drill runs on the scripted meeting-brief /
//! meeting-invite worlds; sends are never real (a `FileOutbox` in
//! the test out dir).

#![cfg(test)]

use super::task_edit::edit_step;
use super::task_view::{detail, list_summaries};
use super::task_worker::{run_worker, KillPoint, WorkerOpts, WorkerReport};
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_eval::World;
use hatch_plan::{latest_plan_approval, PlanApproveExecutor, PlanStatus, PlanStore};
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{EventLog, StepEdit, TaskCommands, TaskStatus, TaskStore, UnitStore};
use hatch_tools::{execute_approved, ExecOutcome, ExecutorRegistry, SendEmailExecutor, WriteFileExecutor};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief")
}

pub(crate) fn setup() -> (Arc<Db>, PathBuf, TempDir, Timestamp, String) {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let dir = world_dir();
    let world = World::load(&dir).unwrap();
    let now = parse_rfc3339(&world.persona.now).unwrap();
    let account = world.persona.email.clone();
    (db, dir, TempDir::new().unwrap(), now, account)
}

pub(crate) fn opts(task_id: Option<String>, kind: Option<&str>, now: Timestamp, kill: KillPoint) -> WorkerOpts {
    WorkerOpts {
        task_id,
        kind: kind.map(|k| k.to_string()),
        lease_ttl_secs: 1,
        now_override: Some(now),
        kill,
        router: None,
        max_model_calls: None,
    }
}

/// Run a fresh brief task until it blocks on the artifact approval:
/// triage + gather done, artifact gated.
pub(crate) fn run_blocked(db: &Arc<Db>, dir: &Path, out: &TempDir, now: Timestamp) -> WorkerReport {
    let report = run_worker(db, dir, out.path(), opts(None, None, now, KillPoint::None)).unwrap();
    assert!(report.blocked.is_some(), "第一轮应在审批上暂停");
    assert_eq!(report.steps_done, 2, "triage + gather 已完成，artifact 待批");
    report
}

/// Approve a write approval and execute it (the `hatch approvals
/// approve` path, in-process): a merely-decided approval leaves the
/// worker blocked on it, so the execution is part of the approval.
pub(crate) fn approve_write(db: &Db, id: &str, now: Timestamp) {
    let q = ApprovalQueue::new(db);
    let status = q.decide(id, true, "drill approve write", &now).unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(WriteFileExecutor));
    let outcome = execute_approved(db, &exec, id, &now).unwrap();
    assert!(
        matches!(outcome, ExecOutcome::Executed(_)),
        "审批执行应成功，实际 {outcome:?}"
    );
}

/// Run the worker with a kill point: it returns Err (the simulated
/// crash); the task id comes from the open-task list (the killed run
/// is the only open task at that point).
pub(crate) fn run_killed(db: &Arc<Db>, dir: &Path, out: &TempDir, now: Timestamp, kill: KillPoint) -> String {
    let err = run_worker(db, dir, out.path(), opts(None, None, now, kill)).unwrap_err();
    assert!(
        err.to_string().contains("drill: worker terminated"),
        "应为模拟崩溃，实际 {err}"
    );
    TaskStore::new(db).open().unwrap().pop().unwrap().id
}

/// The artifact step's recorded approval id.
pub(crate) fn artifact_approval_id(db: &Db, task_id: &str) -> String {
    UnitStore::new(db)
        .get_step(task_id, "artifact")
        .unwrap()
        .unwrap()
        .approval_id
}

/// Approve the task's plan (the `hatch plan approve` path, in-process).
pub(crate) fn approve_plan(db: &Db, plan_id: &str, now: Timestamp) {
    let plan = PlanStore::new(db).get(plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    let entry = latest_plan_approval(db, &plan)
        .unwrap()
        .expect("计划应有待审批记录");
    let q = ApprovalQueue::new(db);
    let status = q
        .decide(&entry.id, true, "drill approve plan", &now)
        .unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(PlanApproveExecutor::new("drill")));
    let outcome = execute_approved(db, &exec, &entry.id, &now).unwrap();
    assert!(
        matches!(outcome, hatch_tools::ExecOutcome::Executed(_)),
        "计划审批执行应成功，实际 {outcome:?}"
    );
    let plan = PlanStore::new(db).get(plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Approved);
}

/// Approve the send-invite approval and execute it through a
/// `FileOutbox` in the test out dir (the `hatch approvals approve`
/// path, in-process). Returns the approval id.
pub(crate) fn approve_send(db: &Db, out: &TempDir, account: &str, task_id: &str, now: Timestamp) -> String {
    let units = UnitStore::new(db);
    let step = units.get_step(task_id, "send-invite").unwrap().unwrap();
    let id = step.approval_id.clone();
    assert!(!id.is_empty(), "send-invite 步骤应已记录审批 id");
    let q = ApprovalQueue::new(db);
    let status = q.decide(&id, true, "drill approve send", &now).unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let outbox = Arc::new(FileOutbox::new(out.path(), account).unwrap());
    let exec = ExecutorRegistry::new().with(Arc::new(SendEmailExecutor::new(outbox, account)));
    let outcome = execute_approved(db, &exec, &id, &now).unwrap();
    assert!(
        matches!(outcome, hatch_tools::ExecOutcome::Executed(_)),
        "发送审批执行应成功，实际 {outcome:?}"
    );
    id
}

pub(crate) fn outbox_mails(out: &TempDir) -> Vec<String> {
    std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".eml"))
        .collect()
}

/// The list is one projection of the ledger: every task appears
/// exactly once, with its step progress and pending-approval count.
#[test]
fn list_shows_each_task_once_with_progress_and_approvals() {
    let (db, dir, out, now, _) = setup();
    let store = TaskStore::new(&db);
    let b = store
        .create("meeting-brief", "第二份简报", "daemon", "", 3, None, &now)
        .unwrap();
    let c = store
        .create("meeting-invite", "发邀请", "daemon", "", 3, None, &now)
        .unwrap();
    // One brief runs to its approval (a fresh task); the other two
    // stay fresh.
    let blocked = run_blocked(&db, &dir, &out, now);
    let a_id = blocked.task_id.clone();

    let rows = list_summaries(&db, None, 50).unwrap();
    assert_eq!(rows.len(), 3, "三个任务应各出现一次");
    let ids: HashSet<_> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids.len(), 3, "列表不得有重复行");
    assert!(ids.contains(a_id.as_str()) && ids.contains(b.id.as_str()));
    assert!(ids.contains(c.id.as_str()));
    let row_a = rows.iter().find(|r| r.id == a_id).unwrap();
    assert_eq!((row_a.steps_done, row_a.steps_total), (2, 3));
    assert_eq!(row_a.pending_approvals, 1);
    assert_eq!(row_a.plan_version, 1);
    let row_b = rows.iter().find(|r| r.id == b.id).unwrap();
    assert_eq!((row_b.steps_done, row_b.steps_total), (0, 0));
    assert_eq!(row_b.pending_approvals, 0);

    // The status filter reads the same projection.
    assert_eq!(list_summaries(&db, Some(TaskStatus::Blocked), 50).unwrap().len(), 1);
    assert_eq!(list_summaries(&db, Some(TaskStatus::Done), 50).unwrap().len(), 0);

    // The detail is the plan card: steps with expected, the current
    // step, and the pending approval.
    let d = detail(&db, &a_id).unwrap();
    assert_eq!(d.status, "blocked");
    assert_eq!(d.steps.len(), 3);
    assert_eq!(d.current_step, "artifact");
    assert_eq!(d.steps_done, 2);
    assert!(!d.steps[0].expected.is_empty(), "步骤应带 expected");
    assert_eq!(d.approvals.len(), 1);
    assert_eq!(d.approvals[0].status, "pending");
    assert_eq!(d.approvals[0].step_id, "artifact");
}

/// Two edits racing on the same expected version: the first wins at
/// the bump, the second is rejected as stale, and the step keeps the
/// first edit's values.
#[test]
fn concurrent_edits_only_one_wins() {
    let (db, dir, out, now, _) = setup();
    let task_id = run_killed(&db, &dir, &out, now, KillPoint::AfterStep("triage"));
    let triage = UnitStore::new(&db).get_step(&task_id, "triage").unwrap().unwrap();
    assert_eq!(
        triage.status,
        hatch_tasks::ExecStatus::Done,
        "triage 已完成，gather 待执行"
    );

    let edit_a = StepEdit {
        title: Some("汇总会议资料（修订 A）".into()),
        expected: Some("资料已汇总（修订 A）".into()),
        ..Default::default()
    };
    let ok = edit_step(&db, &task_id, "gather", 1, &edit_a, "user", &now).unwrap();
    assert!(ok.contains("v1 → v2"), "首次编辑应推进版本，实际 {ok}");
    let edit_b = StepEdit {
        title: Some("汇总会议资料（修订 B）".into()),
        expected: Some("资料已汇总（修订 B）".into()),
        ..Default::default()
    };
    let err = edit_step(&db, &task_id, "gather", 1, &edit_b, "user", &now).unwrap_err();
    assert!(
        err.to_string().contains("v2"),
        "并发的第二次编辑应因版本过期被拒，实际 {err}"
    );
    let step = UnitStore::new(&db).get_step(&task_id, "gather").unwrap().unwrap();
    assert_eq!(step.title, "汇总会议资料（修订 A）");
    assert_eq!(step.expected, "资料已汇总（修订 A）");
    let task = TaskStore::new(&db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.plan_version, 2);
    // The command ledger records one applied and one rejected entry.
    let commands = TaskCommands::new(&db).for_task(&task_id).unwrap();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands.iter().filter(|c| c.status == "applied").count(), 1);
    assert_eq!(commands.iter().filter(|c| c.status == "rejected").count(), 1);
}

/// A done step cannot be rewritten; a pending step can; a running
/// step gated on a still-pending approval can — its side effect has
/// not executed yet.
#[test]
fn edit_executed_step_rejected_and_gated_step_editable() {
    let (db, dir, out, now, _) = setup();
    let task_id = run_killed(&db, &dir, &out, now, KillPoint::AfterStep("triage"));

    let done = edit_step(
        &db,
        &task_id,
        "triage",
        1,
        &StepEdit { title: Some("改已执行步骤".into()), ..Default::default() },
        "user",
        &now,
    )
    .unwrap_err();
    assert!(
        done.to_string().contains("已是 done"),
        "已执行步骤应被拒，实际 {done}"
    );
    let ok = edit_step(
        &db,
        &task_id,
        "gather",
        1,
        &StepEdit { expected: Some("资料已汇总（新版）".into()), ..Default::default() },
        "user",
        &now,
    )
    .unwrap();
    assert!(ok.contains("v1 → v2"), "待执行步骤应可编辑，实际 {ok}");

    // Run on: gather executes, artifact dispatches its approval.
    // The clock must advance past the killed run's 1s lease, or the
    // task is still running under a live lease and cannot be taken
    // over.
    let later = now + chrono::Duration::seconds(2);
    let second = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), None, later, KillPoint::None),
    )
    .unwrap();
    assert!(second.blocked.is_some(), "第二轮应在 artifact 审批上暂停");
    let old_id = artifact_approval_id(&db, &task_id);
    assert!(!old_id.is_empty());

    // The gated artifact step is editable; the old approval dies.
    let ok = edit_step(
        &db,
        &task_id,
        "artifact",
        2,
        &StepEdit { title: Some("写简报文件（已修订）".into()), ..Default::default() },
        "user",
        &now,
    )
    .unwrap();
    assert!(ok.contains("v2 → v3"), "待审批步骤应可编辑，实际 {ok}");
    assert_eq!(
        ApprovalQueue::new(&db).get(&old_id).unwrap().status,
        ApprovalStatus::Invalidated,
        "旧审批应随版本失效"
    );
    assert!(
        artifact_approval_id(&db, &task_id).is_empty(),
        "编辑应清空步骤的审批引用，下次续跑重新排队"
    );
}

/// The full edit → resume flow: an edited gated step resumes on a
/// FRESH approval (the old key is dead), with zero extra model
/// calls, and finishes 3/3 after approval.
#[test]
fn edit_gated_step_then_resume_gets_fresh_approval() {
    let (db, dir, out, now, _) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let task_id = first.task_id.clone();
    let old_id = artifact_approval_id(&db, &task_id);
    assert_eq!(first.model_calls, 9, "第一轮应调 9 次脚本模型");

    let ok = edit_step(
        &db,
        &task_id,
        "artifact",
        1,
        &StepEdit {
            title: Some("写简报文件（已修订）".into()),
            expected: Some("简报已写入（修订版）".into()),
            ..Default::default()
        },
        "user",
        &now,
    )
    .unwrap();
    assert!(ok.contains("v1 → v2"), "编辑应推进版本，实际 {ok}");
    assert_eq!(
        ApprovalQueue::new(&db).get(&old_id).unwrap().status,
        ApprovalStatus::Invalidated
    );

    // Resume: a new approval is queued (a different id), no model
    // calls, steps still 2/3.
    let second = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), None, now, KillPoint::None),
    )
    .unwrap();
    assert_eq!(second.model_calls, 0, "续跑不得再调模型");
    assert_eq!(second.steps_done, 2);
    assert!(second.blocked.is_some(), "续跑应在新审批上暂停");
    let new_id = artifact_approval_id(&db, &task_id);
    assert!(!new_id.is_empty(), "续跑应记录新审批 id");
    assert_ne!(new_id, old_id, "续跑应排一个新审批，而不是复用已失效的");

    // Approve the fresh approval; the resume finishes 3/3 and
    // writes exactly one brief.
    approve_write(&db, &new_id, now);
    let third = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), None, now, KillPoint::None),
    )
    .unwrap();
    assert_eq!(third.steps_done, 3);
    assert!(third.blocked.is_none());
    assert_eq!(third.artifact, Some(out.path().join("brief-v1.md")));
    assert!(out.path().join("brief-v1.md").exists());
    let task = TaskStore::new(&db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.plan_version, 2);
    let done = EventLog::new(&db)
        .events_since(&task_id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "step_done")
        .count();
    assert_eq!(done, 3, "每步恰好一个 step_done 事件");
}

