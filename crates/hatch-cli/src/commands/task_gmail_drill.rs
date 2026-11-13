//! Drills for the M2 Gmail pilot: the worker core runs on an
//! injected mail source and a mock sender, so a real-account run is
//! the same code path as the fixture run — no second triage chain,
//! no approval bypass. Sends are never real: a `FakeSender` records
//! them in memory, and the fixture `FileOutbox` is not involved at
//! all (no `.eml` file appears in the out dir).
//!
//! - brief: sync (fake source) → triage → gather → brief, with the
//!   model input in code space and the delivered brief in real names.
//! - invite: plan approval → send approval → exactly one recorded
//!   send; unapproved, zero sends. The plan has no read-calendar
//!   step (a Gmail-only pilot has no calendar).

#![cfg(test)]

use super::task_desensitize_drill::{real_addresses, session_messages};
use super::task_invite::InviteSpec;
use super::task_list_drill::{approve_plan, approve_write, artifact_approval_id, opts, setup};
use super::task_worker::{run_worker_core, KillPoint, RunInput, WorkerReport};
use hatch_connectors::{MailSender, MailSource, OutgoingMail, ParsedMail};
use hatch_core::time::{parse_tz, Timestamp};
use hatch_eval::World;
use hatch_plan::{PlanStatus, PlanStore};
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{EventLog, TaskStatus, TaskStore, UnitStore};
use hatch_tools::{execute_approved, ExecutorRegistry, SendEmailExecutor};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// A mock `MailSource`: returns the seeded mails once, then nothing
/// (like a cursor that never advances past what it already saw).
struct FakeMailbox {
    account: String,
    mails: Vec<ParsedMail>,
    spent: bool,
}

impl FakeMailbox {
    fn new(account: &str, world: &World) -> Self {
        FakeMailbox {
            account: account.to_string(),
            mails: world
                .mailbox
                .iter()
                .map(|m| m.to_parsed().unwrap())
                .collect(),
            spent: false,
        }
    }
}

impl MailSource for FakeMailbox {
    fn account(&self) -> String {
        self.account.clone()
    }

    fn fetch_new(
        &mut self,
        _after_uid: Option<u32>,
        limit: usize,
    ) -> hatch_core::Result<Vec<ParsedMail>> {
        if self.spent {
            return Ok(vec![]);
        }
        self.spent = true;
        Ok(self.mails.iter().take(limit).cloned().collect())
    }
}

/// A mock `MailSender`: records every send in memory, returns a
/// fake id. No network, no files.
struct FakeSender {
    sent: Arc<Mutex<Vec<OutgoingMail>>>,
}

impl MailSender for FakeSender {
    fn send(&self, mail: &OutgoingMail) -> hatch_core::Result<String> {
        let mut sent = self.sent.lock().unwrap();
        sent.push(mail.clone());
        Ok(format!("fake-{}", sent.len()))
    }
}

/// Approve the send-invite approval and execute it through the fake
/// sender (the `hatch approvals approve` path, in-process). Returns
/// the approval id.
fn approve_send_fake(
    db: &Db,
    sender: &Arc<dyn MailSender>,
    account: &str,
    task_id: &str,
    now: Timestamp,
) -> String {
    let units = UnitStore::new(db);
    let step = units.get_step(task_id, "send-invite").unwrap().unwrap();
    let id = step.approval_id.clone();
    assert!(!id.is_empty(), "send-invite 步骤应已记录审批 id");
    let q = ApprovalQueue::new(db);
    let status = q.decide(&id, true, "drill approve send", &now).unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(SendEmailExecutor::new(
        Arc::clone(sender),
        account,
    )));
    let outcome = execute_approved(db, &exec, &id, &now).unwrap();
    assert!(
        matches!(outcome, hatch_tools::ExecOutcome::Executed(_)),
        "发送审批执行应成功，实际 {outcome:?}"
    );
    id
}

fn brief_input() -> RunInput {
    RunInput {
        title: Some("简报：最近邮件".into()),
        prompt: Some("把最近邮件里的决定整理成简报，每条注明来源邮件".into()),
        invite_spec: None,
        invite_sender: None,
        with_calendar: false,
        source: "gmail",
    }
}

fn invite_input(spec: &InviteSpec, sender: &Arc<dyn MailSender>) -> RunInput {
    RunInput {
        title: Some("邀请：项目评审会".into()),
        prompt: None,
        invite_spec: Some(spec.clone()),
        invite_sender: Some(Arc::clone(sender)),
        with_calendar: false,
        source: "gmail",
    }
}

/// A resume run passes no spec: the worker rebuilds it from the
/// approved plan row. The sender stays the fake one.
fn resume_input(sender: &Arc<dyn MailSender>) -> RunInput {
    RunInput {
        title: None,
        prompt: None,
        invite_spec: None,
        invite_sender: Some(Arc::clone(sender)),
        with_calendar: false,
        source: "gmail",
    }
}

#[allow(clippy::too_many_arguments)]
fn run_core(
    db: &Arc<Db>,
    world: &World,
    dir: &Path,
    out: &TempDir,
    task_id: Option<String>,
    kind: Option<&str>,
    now: Timestamp,
    source: &mut dyn MailSource,
    input: RunInput,
) -> WorkerReport {
    run_worker_core(
        db,
        world,
        Some(dir),
        out.path(),
        opts(task_id, kind, now, KillPoint::None),
        source,
        input,
    )
    .unwrap()
}

#[test]
fn gmail_brief_via_injected_source_completes_with_real_names() {
    let (db, dir, out, now, account) = setup();
    let world = World::load(&dir).unwrap();
    let addrs = real_addresses(&world);

    let blocked = run_core(
        &db,
        &world,
        &dir,
        &out,
        None,
        None,
        now,
        &mut FakeMailbox::new(&account, &world),
        brief_input(),
    );
    assert!(blocked.blocked.is_some(), "第一轮应在审批上暂停");
    assert_eq!(
        blocked.steps_done, 2,
        "triage + gather 已完成，artifact 待批"
    );
    // The task records its mail source: an approval bound to it later
    // executes with the Gmail sender, not the fixture outbox.
    let task = TaskStore::new(&db).get(&blocked.task_id).unwrap().unwrap();
    assert_eq!(task.source, "gmail");

    let messages = session_messages(&db);
    assert!(!messages.is_empty(), "gather 应留下会话记录");
    for (role, content) in &messages {
        for a in &addrs {
            assert!(
                !content.contains(a.as_str()),
                "模型输入含真实地址 {a}（role={role}）"
            );
        }
    }
    let joined: String = messages.iter().map(|(_, c)| c.as_str()).collect();
    assert!(joined.contains("@example.invalid"), "模型输入应含代号地址");
    for (role, content) in &messages {
        if role == "tool" {
            assert!(
                !content.contains("李婷"),
                "工具结果不得含真实姓名，实际：{content}"
            );
        }
    }

    // Approve the artifact write and run to completion; the
    // delivered brief shows real names and no codes.
    let approval_id = artifact_approval_id(&db, &blocked.task_id);
    approve_write(&db, &approval_id, now);
    let done = run_core(
        &db,
        &world,
        &dir,
        &out,
        Some(blocked.task_id.clone()),
        None,
        now,
        &mut FakeMailbox::new(&account, &world),
        RunInput::default(),
    );
    assert_eq!(done.steps_done, 3);
    assert!(done.blocked.is_none());
    let brief = std::fs::read_to_string(out.path().join("brief-v1.md")).unwrap();
    assert!(brief.contains("李婷"), "简报应显示真实姓名");
    assert!(
        brief.contains("方案 B 通过") && brief.contains("预算上限 5 万元已确认"),
        "简报应含决定，实际：{brief}"
    );
    assert!(
        !brief.contains("@example.invalid"),
        "简报不得含代号地址，实际：{brief}"
    );
    assert!(
        !brief.contains("发件人A") && !brief.contains("发件人B"),
        "简报不得含代号姓名"
    );
}

#[test]
fn gmail_invite_unapproved_sends_nothing_and_approved_sends_once() {
    let (db, dir, out, now, account) = setup();
    let world = World::load(&dir).unwrap();
    let tz = parse_tz(&world.persona.timezone).unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sender: Arc<dyn MailSender> = Arc::new(FakeSender {
        sent: Arc::clone(&sent),
    });
    let spec = InviteSpec::from_args(
        "项目评审会",
        "2026-11-04 10:00",
        "3 号会议室",
        "李婷",
        "li.ting@corp.example",
        "方案 B 通过；预算上限 5 万元",
        &world.persona.name,
        &now,
        &tz,
    )
    .unwrap();

    // Phase 1: the worker builds the plan and blocks on its
    // approval — no send.
    let first = run_core(
        &db,
        &world,
        &dir,
        &out,
        None,
        Some("meeting-invite"),
        now,
        &mut FakeMailbox::new(&account, &world),
        invite_input(&spec, &sender),
    );
    assert_eq!(first.steps_done, 0, "计划未批前不执行任何步骤");
    assert!(first.blocked.is_some(), "第一轮应在计划审批上暂停");
    assert_eq!(first.model_calls, 0, "邀请不调模型");
    assert!(sent.lock().unwrap().is_empty(), "计划未批前不得发送");
    let task = TaskStore::new(&db).get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.kind, "meeting-invite");
    assert_eq!(task.source, "gmail", "任务应记录真实来源");
    assert!(!task.plan_id.is_empty(), "任务应已关联计划");
    let plan = PlanStore::new(&db).get(&task.plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    assert_eq!(plan.steps.len(), 2, "Gmail 模式没有 read-calendar 步骤");
    assert_eq!(
        plan.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["read-mail", "send-invite"]
    );

    // Phase 2: approve the plan; the worker reads + queues the send,
    // then blocks on the send approval — still no send. The resume
    // passes no spec: it is rebuilt from the approved plan row.
    approve_plan(&db, &task.plan_id, now);
    let second = run_core(
        &db,
        &world,
        &dir,
        &out,
        Some(first.task_id.clone()),
        Some("meeting-invite"),
        now,
        &mut FakeMailbox::new(&account, &world),
        resume_input(&sender),
    );
    assert_eq!(second.steps_done, 1, "read-mail 已完成");
    assert!(second.blocked.is_some(), "第二轮应在发送审批上暂停");
    assert!(sent.lock().unwrap().is_empty(), "批准发送前不得有任何邮件");

    // Phase 3: approve + execute the send through the fake sender;
    // the worker finishes from the breakpoint without re-sending.
    // Again no spec on the resume: the plan row is authoritative.
    let send_id = approve_send_fake(&db, &sender, &account, &first.task_id, now);
    let report = run_core(
        &db,
        &world,
        &dir,
        &out,
        Some(first.task_id.clone()),
        Some("meeting-invite"),
        now,
        &mut FakeMailbox::new(&account, &world),
        resume_input(&sender),
    );
    assert_eq!(report.steps_done, 2);
    assert!(report.blocked.is_none());
    let task = TaskStore::new(&db).get(&report.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    let plan = PlanStore::new(&db).get(&task.plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Completed);
    // Exactly one send, to the work address only, with the meeting
    // details — and no simulated mail file anywhere: the FileOutbox
    // was never constructed for this task.
    let sent = sent.lock().unwrap();
    assert_eq!(sent.len(), 1, "恰好发一封");
    assert_eq!(sent[0].to, vec!["li.ting@corp.example"], "收件人是工作邮箱");
    assert!(sent[0].subject.contains("项目评审会"), "邮件须提及会议");
    assert!(sent[0].body.contains("3 号会议室"), "邮件须注明地点");
    assert!(sent[0].body.contains("方案 B"), "邮件须注明议题");
    assert!(sent[0].body.contains("2026-11-04 10:00"), "邮件须注明时间");
    drop(sent);
    assert!(
        !std::fs::read_dir(out.path()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".eml")),
        "不得有模拟邮件文件"
    );
    // The send step carries its approval id and its ledger receipt.
    let step = UnitStore::new(&db)
        .get_step(&report.task_id, "send-invite")
        .unwrap()
        .unwrap();
    assert_eq!(step.approval_id, send_id);
    assert!(
        !step.receipt_id.is_empty(),
        "send-invite 步骤应记录发送回执（ledger key）"
    );
    // Exactly one step_done event per step.
    let done = EventLog::new(&db)
        .events_since(&report.task_id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "step_done")
        .count();
    assert_eq!(done, 2);
}
