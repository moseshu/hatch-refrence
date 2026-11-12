//! Drills for the meeting-invite kind (M1, task 4): plan → approval
//! → send, with the worker pausing on each approval and resuming from
//! the breakpoint. Sends are simulated (a `FileOutbox` in the test
//! out dir), never real; the invite goes to the work address
//! `li.ting@corp.example`, never the same-name personal one.

#![cfg(test)]

use super::task_worker::{run_worker, KillPoint, WorkerOpts, WorkerReport};
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_eval::World;
use hatch_plan::{latest_plan_approval, PlanApproveExecutor, PlanStatus, PlanStore};
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{EventLog, TaskApprovals, TaskStatus, TaskStore, UnitStore};
use hatch_tools::{execute_approved, ExecutorRegistry, SendEmailExecutor};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief")
}

fn setup() -> (Arc<Db>, PathBuf, TempDir, Timestamp, String) {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let dir = world_dir();
    let world = World::load(&dir).unwrap();
    let now = parse_rfc3339(&world.persona.now).unwrap();
    let account = world.persona.email.clone();
    (db, dir, TempDir::new().unwrap(), now, account)
}

fn opts(task_id: Option<String>, now: Timestamp) -> WorkerOpts {
    WorkerOpts {
        task_id,
        kind: Some("meeting-invite".into()),
        lease_ttl_secs: 1,
        now_override: Some(now),
        kill: KillPoint::None,
        router: None,
        max_model_calls: None,
    }
}

/// Approve the task's plan (the `hatch plan approve` path, in-process).
fn approve_plan(db: &Db, plan_id: &str, now: Timestamp) {
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
fn approve_send(db: &Db, out: &TempDir, account: &str, task_id: &str, now: Timestamp) -> String {
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

fn outbox_mails(out: &TempDir) -> Vec<String> {
    std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|f| f.ends_with(".eml"))
        .collect()
}

#[test]
fn invite_runs_plan_approval_send_and_finishes() {
    let (db, dir, out, now, account) = setup();
    // Phase 1: the worker builds the plan and blocks on its approval.
    let first: WorkerReport = run_worker(&db, &dir, out.path(), opts(None, now)).unwrap();
    assert_eq!(first.steps_done, 0, "计划未批前不执行任何步骤");
    assert!(first.blocked.is_some(), "第一轮应在计划审批上暂停");
    assert_eq!(first.model_calls, 0, "邀请不调模型");
    let store = TaskStore::new(&db);
    let task = store.get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.kind, "meeting-invite");
    assert!(!task.plan_id.is_empty(), "任务应已关联计划");
    let plan = PlanStore::new(&db).get(&task.plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    assert_eq!(plan.steps.len(), 3);
    assert_eq!(
        plan.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["read-calendar", "read-mail", "send-invite"]
    );

    // Phase 2: approve the plan; the worker reads + queues the send,
    // then blocks on the send approval — no mail yet.
    approve_plan(&db, &task.plan_id, now);
    let second = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    )
    .unwrap();
    assert_eq!(second.steps_done, 2, "read-calendar + read-mail 已完成");
    assert!(second.blocked.is_some(), "第二轮应在发送审批上暂停");
    assert!(
        outbox_mails(&out).is_empty(),
        "批准发送前不得有任何邮件文件"
    );
    let task = store.get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);

    // Phase 3: approve + execute the send; the worker finishes from
    // the breakpoint without re-reading or re-sending.
    let send_id = approve_send(&db, &out, &account, &first.task_id, now);
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(first.task_id.clone()), now),
    )
    .unwrap();
    assert_eq!(report.steps_done, 3);
    assert!(report.blocked.is_none());
    let task = store.get(&report.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    let plan = PlanStore::new(&db).get(&task.plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Completed);
    // Exactly one simulated send, to the work address only. The
    // `.eml` is MIME-encoded (encoded-word subject, base64 body), so
    // parse it back instead of grepping the raw file.
    let mails = outbox_mails(&out);
    assert_eq!(mails.len(), 1, "恰好一封模拟发送的邮件");
    let raw = std::fs::read(out.path().join(&mails[0])).unwrap();
    let parsed = hatch_connectors::mail::parse::parse_rfc822(&raw, None).unwrap();
    assert_eq!(parsed.to, vec!["li.ting@corp.example"], "收件人是工作邮箱");
    assert!(
        !parsed.to.iter().any(|t| t.contains("personal")),
        "同名个人邮箱不得收到邀请"
    );
    assert!(parsed.subject.contains("项目评审会"), "邮件须提及会议");
    assert!(parsed.body.contains("3 号会议室"), "邮件须注明地点");
    assert!(parsed.body.contains("方案 B"), "邮件须注明议题");
    assert!(parsed.body.contains("2026-11-04 10:00"), "邮件须注明时间");
    // The send step carries its approval id and its ledger receipt.
    let units = UnitStore::new(&db);
    let step = units
        .get_step(&report.task_id, "send-invite")
        .unwrap()
        .unwrap();
    assert_eq!(step.approval_id, send_id);
    assert!(
        !step.receipt_id.is_empty(),
        "send-invite 步骤应记录发送回执（ledger key）"
    );
    // The approval is linked to the task at its plan version.
    let linked = TaskApprovals::new(&db).linked_version(&send_id).unwrap();
    assert_eq!(linked, Some((report.task_id.clone(), 1)));
    // Exactly one step_done event per step.
    let done = EventLog::new(&db)
        .events_since(&report.task_id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "step_done")
        .count();
    assert_eq!(done, 3);
}
