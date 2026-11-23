//! P1 plan drill: a submitted plan draft runs through the generic
//! Plan-and-Execute runner — plan approval, then each `fs_write`
//! step through its own approval, then Done. The plan is a draft
//! the user submitted in chat, not a fixture: the steps and their
//! contents come from the submit args only.

use super::p1_submit_drill::{
    drive_to_done, only_task, run_chat_submit, setup, status, sweep_detail,
};
use crate::app::App;
use crate::commands::task_resident::ResidentWorker;
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_connectors::MailSender;
use hatch_core::time::now;
use hatch_plan::PlanApproveExecutor;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_tasks::{TaskApprovals, TaskStatus, TaskStore};
use hatch_tools::{
    execute_approved, CreateEventExecutor, ExecutorRegistry, SendEmailExecutor, WriteFileExecutor,
};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn chat_submit_plan_runs_each_step_through_approval() {
    let (app, _home) = setup();
    let report = run_chat_submit(
        &app,
        "把两句话分别写进两个文件：第一句写进 a.txt，第二句写进 b.txt",
        json!({
            "goal": "把两句话分别写进两个文件",
            "steps": [
                {"id": "w1", "title": "写第一句", "tool": "fs_write",
                 "arguments": {"path": "a.txt", "content": "第一句"}, "expected": "a.txt 已写入"},
                {"id": "w2", "title": "写第二句", "tool": "fs_write",
                 "arguments": {"path": "b.txt", "content": "第二句"}, "expected": "b.txt 已写入"}
            ],
        }),
    );
    assert_eq!(report.stop, hatch_agent::StopReason::Answered);
    let task_id = only_task(&app);
    let task = TaskStore::new(&app.db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.kind, "plan");
    let mut worker = ResidentWorker::new(&app);
    assert!(
        drive_to_done(&app, &mut worker, &task_id),
        "计划任务应在审批流转后完成；{}",
        sweep_detail(&app, &task_id)
    );
    let out_dir = app.home.join("task-out").join(&task_id);
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a.txt")).unwrap(),
        "第一句"
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("b.txt")).unwrap(),
        "第二句"
    );
}

/// Approve + execute every pending approval linked to the task with
/// the same executor set the plan runner uses — crucially the same
/// `FileOutbox` in the task's output dir, so the sent-mail readback
/// at finish time reads the store the send wrote to (the invite
/// drill does the same).
fn approve_pending_matching(app: &Arc<App>, task_id: &str, from: &str) {
    let out_dir = app.home.join("task-out").join(task_id);
    let sender: Arc<dyn MailSender> = Arc::new(FileOutbox::new(&out_dir, from).unwrap());
    let exec = ExecutorRegistry::new()
        .with(Arc::new(PlanApproveExecutor::new("worker")))
        .with(Arc::new(CreateEventExecutor))
        .with(Arc::new(SendEmailExecutor::new(sender, from)))
        .with(Arc::new(WriteFileExecutor));
    let q = ApprovalQueue::new(&app.db);
    for link in TaskApprovals::new(&app.db).for_task(task_id).unwrap() {
        if q.get(&link.approval_id).unwrap().status == ApprovalStatus::Pending {
            q.decide(&link.approval_id, true, "drill approve", &now()).unwrap();
            let _ = execute_approved(&app.db, &exec, &link.approval_id, &now());
        }
    }
}

/// Sweep + approve until the task is Done or the deadline passes.
fn drive_plan(app: &Arc<App>, worker: &mut ResidentWorker, task_id: &str, from: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match status(&app.db, task_id) {
            TaskStatus::Done => return true,
            TaskStatus::Failed | TaskStatus::Cancelled => return false,
            _ => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = worker.sweep_once();
        if status(&app.db, task_id) == TaskStatus::Blocked {
            approve_pending_matching(app, task_id, from);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn outbox_count(app: &Arc<App>, task_id: &str, from: &str) -> usize {
    let out_dir = app.home.join("task-out").join(task_id);
    FileOutbox::new(&out_dir, from)
        .unwrap()
        .entries()
        .unwrap()
        .len()
}

/// Submit a calendar+mail plan whose acceptance contract pins the
/// mail's stated meeting time to the event start; returns task id.
fn submit_event_mail_plan(app: &Arc<App>, mail_time: &str) -> String {
    let body = format!(
        "邀请您参加项目评审会。\n会议时间：{mail_time}\n地点：3 号会议室，请准时参加。"
    );
    let report = run_chat_submit(
        app,
        "建一个项目评审会日程，再给李婷发邀请邮件，邮件里写明会议时间",
        json!({
            "goal": "创建项目评审会日程并发送邀请邮件",
            "steps": [
                {"id": "s1", "title": "建日程", "tool": "propose_event",
                 "arguments": {"title": "项目评审会", "start": "2026-11-23T10:00:00+08:00",
                               "end": "2026-11-23T11:00:00+08:00", "location": "3 号会议室"},
                 "expected": "日程已创建"},
                {"id": "s2", "title": "发邀请邮件", "tool": "propose_email",
                 "arguments": {"to": ["li.ting@corp.example"], "subject": "邀请：项目评审会",
                               "body": body},
                 "expected": "邀请邮件已发送"}
            ],
            "acceptance_items": [
                {"kind": "predicate", "id": "cal-mail-time",
                 "left": {"step": "s1", "field": "event.starts_at"}, "op": "equals",
                 "right": {"kind": "field", "step": "s2", "field": "mail.meeting_time"},
                 "normalize": "rfc3339_tz"}
            ],
        }),
    );
    assert_eq!(report.stop, hatch_agent::StopReason::Answered);
    only_task(app)
}

#[test]
fn plan_mail_step_sends_and_acceptance_passes_when_consistent() {
    let (app, _home) = setup();
    let from = app.user_email(); // synthetic-world persona = config user email
    let task_id = submit_event_mail_plan(&app, "2026-11-23T10:00:00+08:00");
    let mut worker = ResidentWorker::new(&app);
    assert!(
        drive_plan(&app, &mut worker, &task_id, &from),
        "一致的计划任务应完成；{}",
        sweep_detail(&app, &task_id)
    );
    assert_eq!(outbox_count(&app, &task_id, &from), 1, "应恰好发出一封邮件");
}

#[test]
fn plan_mail_step_contradiction_is_blocked_by_enforced_l2() {
    let (app, _home) = setup();
    let from = app.user_email();
    // Mail says 11:00, event starts 10:00 — L2 is enforced by
    // default, so the task must not complete. The mail itself was
    // approved and sent exactly once; recovery must not resend.
    let task_id = submit_event_mail_plan(&app, "2026-11-23T11:00:00+08:00");
    let mut worker = ResidentWorker::new(&app);
    assert!(
        !drive_plan(&app, &mut worker, &task_id, &from),
        "矛盾的计划任务不应完成；{}",
        sweep_detail(&app, &task_id)
    );
    assert_eq!(
        outbox_count(&app, &task_id, &from),
        1,
        "邮件应恰好发出一封，恢复不得重发"
    );
}
