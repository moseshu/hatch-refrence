//! Drills for task 5 (pause + plan-step edit + event replay): pause
//! stops at the next boundary and resumes from the breakpoint, a
//! pause is refused on blocked/terminal tasks, a plan-step edit
//! forces re-approval before the invite finishes, and event replay
//! is idempotent. Helpers live in `task_list_drill` and are reused
//! here. Every drill runs on the scripted meeting-brief /
//! meeting-invite worlds; sends are never real (a `FileOutbox` in
//! the test out dir).

#![cfg(test)]

use super::task_control::pause_task;
use super::task_edit::edit_step;
use super::task_list_drill::{
    approve_plan, approve_send, approve_write, artifact_approval_id, opts, outbox_mails,
    run_blocked, run_killed, setup,
};
use super::task_view::list_summaries;
use super::task_worker::{run_worker, KillPoint};
use hatch_plan::{latest_plan_approval, PlanStatus, PlanStore};
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_tasks::{BlockedKind, EventLog, StepEdit, TaskStatus, TaskStore, UnitStore};
use std::collections::HashSet;
/// continues from the breakpoint without re-running a done step.
#[test]
fn pause_stops_and_resumes_from_breakpoint() {
    let (db, dir, out, now, _) = setup();
    let task_id = run_killed(&db, &dir, &out, now, KillPoint::AfterStep("triage"));

    let msg = pause_task(&db, &task_id, "drill pause", &now).unwrap();
    assert!(msg.contains("已暂停"), "暂停应确认，实际 {msg}");
    let task = TaskStore::new(&db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_kind, BlockedKind::Paused);
    let gather = UnitStore::new(&db).get_step(&task_id, "gather").unwrap().unwrap();
    assert_eq!(gather.status, hatch_tasks::ExecStatus::Pending, "暂停不得领取新步骤");
    let paused_events = EventLog::new(&db)
        .events_since(&task_id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "task_paused")
        .count();
    assert_eq!(paused_events, 1);

    // Resume: the worker finishes from the breakpoint — gather
    // executes, then it blocks on the artifact approval (steps 2/3).
    // Approving it finishes the task; triage is not re-run
    // (attempts stays 1).
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), None, now, KillPoint::None),
    )
    .unwrap();
    assert!(!report.paused, "续跑的报告不应标记暂停");
    assert_eq!(report.steps_done, 2, "续跑应从断点跑完 gather，停在 artifact 审批上");
    assert!(report.blocked.is_some(), "续跑应在 artifact 审批上暂停");
    approve_write(&db, &artifact_approval_id(&db, &task_id), now);
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), None, now, KillPoint::None),
    )
    .unwrap();
    assert_eq!(report.steps_done, 3, "批准后续跑应完成剩余步骤");
    let triage = UnitStore::new(&db).get_step(&task_id, "triage").unwrap().unwrap();
    assert_eq!(triage.attempts, 1, "已完成的 triage 不得重跑");
    let task = TaskStore::new(&db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
}

/// A pause is refused on an approval-blocked task and on a terminal
/// task; reconcile never moves a paused task.
#[test]
fn pause_rejected_on_blocked_and_terminal_and_reconcile_leaves_paused_alone() {
    let (db, dir, out, now, _) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let task_id = first.task_id.clone();
    let err = pause_task(&db, &task_id, "drill pause", &now).unwrap_err();
    assert!(
        err.to_string().contains("不能暂停"),
        "审批阻塞的任务应拒绝暂停，实际 {err}"
    );
    // Finish it, then a pause is terminal-refused.
    approve_write(&db, &artifact_approval_id(&db, &task_id), now);
    let done = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), None, now, KillPoint::None),
    )
    .unwrap();
    assert_eq!(done.steps_done, 3);
    let err = pause_task(&db, &task_id, "drill pause", &now).unwrap_err();
    assert!(
        err.to_string().contains("不能暂停"),
        "终态任务应拒绝暂停，实际 {err}"
    );

    // A paused task is untouched by reconcile (it is blocked, not
    // running, and its blocked reason is the pause).
    let paused_id = run_killed(&db, &dir, &out, now, KillPoint::AfterStep("triage"));
    pause_task(&db, &paused_id, "drill pause", &now).unwrap();
    let report = TaskStore::new(&db).reconcile(&now, 0).unwrap();
    assert_eq!(report.examined, 0, "reconcile 只看 running 任务");
    let task = TaskStore::new(&db).get(&paused_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_kind, BlockedKind::Paused);
}

/// Editing a pending plan step forces re-approval: the old plan
/// approval is rejected, a fresh one is queued, the plan and the
/// mirror both carry the edit, and the invite still finishes with
/// exactly one simulated send.
#[test]
fn edit_plan_step_requires_reapproval_then_finishes() {
    let (db, dir, out, now, account) = setup();
    // Phase 1: the worker builds the plan and blocks on its approval.
    let first = run_worker(
        &db,
        &dir,
        out.path(),
        opts(None, Some("meeting-invite"), now, KillPoint::None),
    )
    .unwrap();
    assert_eq!(first.steps_done, 0, "计划未批前不执行任何步骤");
    assert!(first.blocked.is_some(), "第一轮应在计划审批上暂停");
    let task_id = first.task_id.clone();
    let task = TaskStore::new(&db).get(&task_id).unwrap().unwrap();
    let plan_id = task.plan_id.clone();
    let plan = PlanStore::new(&db).get(&plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    let old_entry = latest_plan_approval(&db, &plan).unwrap().unwrap();

    // Edit a pending plan step: version bumps on both rows, the old
    // plan approval is rejected, a fresh one is queued.
    let ok = edit_step(
        &db,
        &task_id,
        "read-mail",
        1,
        &StepEdit { expected: Some("读完相关邮件并摘出时间地点（新版）".into()), ..Default::default() },
        "user",
        &now,
    )
    .unwrap();
    assert!(ok.contains("v1 → v2"), "编辑应推进版本，实际 {ok}");
    let task = TaskStore::new(&db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.plan_version, 2);
    let plan = PlanStore::new(&db).get(&plan_id).unwrap().unwrap();
    assert_eq!(plan.version, 2, "计划版本应与任务版本同步");
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    let pstep = plan.steps.iter().find(|s| s.id == "read-mail").unwrap();
    assert_eq!(pstep.expected, "读完相关邮件并摘出时间地点（新版）");
    let mirror = UnitStore::new(&db).get_step(&task_id, "read-mail").unwrap().unwrap();
    assert_eq!(mirror.expected, "读完相关邮件并摘出时间地点（新版）");
    assert_eq!(
        ApprovalQueue::new(&db).get(&old_entry.id).unwrap().status,
        ApprovalStatus::Rejected,
        "旧计划审批应被新提交拒绝"
    );
    let new_entry = latest_plan_approval(&db, &plan).unwrap().unwrap();
    assert_ne!(new_entry.id, old_entry.id, "应提交一个新的计划审批");
    assert_eq!(new_entry.status, ApprovalStatus::Pending);

    // Phase 2: approve the new plan; the worker reads + queues the
    // send, then blocks on the send approval — no mail yet.
    approve_plan(&db, &plan_id, now);
    let second = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), Some("meeting-invite"), now, KillPoint::None),
    )
    .unwrap();
    assert_eq!(second.steps_done, 2, "read-calendar + read-mail 已完成");
    assert!(second.blocked.is_some(), "第二轮应在发送审批上暂停");
    assert!(outbox_mails(&out).is_empty(), "批准发送前不得有任何邮件文件");

    // A done plan step cannot be rewritten now.
    let done = edit_step(
        &db,
        &task_id,
        "read-calendar",
        2,
        &StepEdit { title: Some("改已执行步骤".into()), ..Default::default() },
        "user",
        &now,
    )
    .unwrap_err();
    assert!(done.to_string().contains("已是 done"), "已执行步骤应被拒，实际 {done}");

    // Phase 3: approve + execute the send; exactly one mail, to the
    // work address only.
    approve_send(&db, &out, &account, &task_id, now);
    let report = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), Some("meeting-invite"), now, KillPoint::None),
    )
    .unwrap();
    assert_eq!(report.steps_done, 3);
    assert!(report.blocked.is_none());
    let task = TaskStore::new(&db).get(&report.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    let mails = outbox_mails(&out);
    assert_eq!(mails.len(), 1, "恰好一封模拟发送的邮件");
    let raw = std::fs::read(out.path().join(&mails[0])).unwrap();
    let parsed = hatch_connectors::mail::parse::parse_rfc822(&raw, None).unwrap();
    assert_eq!(parsed.to, vec!["li.ting@corp.example"], "收件人是工作邮箱");
}

/// Event replay is idempotent: reading the same events twice yields
/// the same rows, and the list projection is stable across reads.
#[test]
fn events_replay_is_idempotent_and_list_is_stable() {
    let (db, dir, out, now, _) = setup();
    let first = run_blocked(&db, &dir, &out, now);
    let task_id = first.task_id.clone();

    let log = EventLog::new(&db);
    let once = log.events_since(&task_id, 0, 1000).unwrap();
    let twice = log.events_since(&task_id, 0, 1000).unwrap();
    assert!(!once.is_empty(), "应有事件可重放");
    assert_eq!(
        once.iter().map(|e| e.seq).collect::<Vec<_>>(),
        twice.iter().map(|e| e.seq).collect::<Vec<_>>(),
        "两次重放应得到相同的事件序列"
    );
    let seqs: HashSet<_> = once.iter().map(|e| e.seq).collect();
    assert_eq!(seqs.len(), once.len(), "重放不得产生重复行");

    let list_a: Vec<String> = list_summaries(&db, None, 50)
        .unwrap()
        .iter()
        .map(|r| r.id.clone())
        .collect();
    let list_b: Vec<String> = list_summaries(&db, None, 50)
        .unwrap()
        .iter()
        .map(|r| r.id.clone())
        .collect();
    assert_eq!(list_a, list_b, "两次列表应一致");
    assert_eq!(list_a.len(), 1);
    // The store itself dedups: listing with a big limit cannot
    // surface the same task twice.
    let raw = TaskStore::new(&db).list(None, 100).unwrap();
    assert_eq!(raw.len(), 1);
}
